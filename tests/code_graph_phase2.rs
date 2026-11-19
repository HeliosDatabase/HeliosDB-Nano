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
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
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
    let rows = db.query("SELECT to_name FROM _hdb_code_symbol_refs WHERE kind = 'IMPORTS'", &[])?;
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
    let rows = db.query("SELECT to_name FROM _hdb_code_symbol_refs WHERE kind = 'IMPORTS'", &[])?;
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
    let rows = db.query("SELECT to_name FROM _hdb_code_symbol_refs WHERE kind = 'IMPORTS'", &[])?;
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
    insert(&db, "helper.py", "python", "def helper():\n    return 42\n")?;
    insert(&db, "caller.py", "python", "def run():\n    return helper()\n")?;
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
// Code-index FK-violation acceptance fixtures (fixed v3.22.1)
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
    (
        "a.rs",
        "rust",
        "pub fn alpha() { beta(); }\npub fn beta() {}\npub struct S;\nimpl S { pub fn m(&self) -> i32 { 0 } }\n",
    ),
    (
        "b.rs",
        "rust",
        "use crate::a::S;\npub fn make() -> S { S }\npub fn caller() { let s = make(); s.m(); }\n",
    ),
    (
        "c.rs",
        "rust",
        "pub trait T { fn run(&self); }\npub struct U;\nimpl T for U { fn run(&self) {} }\n",
    ),
    (
        "d.py",
        "python",
        "def hello():\n    print('hi')\n\nclass Foo:\n    def bar(self):\n        return hello()\n",
    ),
    (
        "e.py",
        "python",
        "from d import Foo\n\ndef use_foo():\n    f = Foo()\n    return f.bar()\n",
    ),
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

// ---------------------------------------------------------------------------
// v09.3: index-eligible shapes for definition, outline and call hierarchy
// ---------------------------------------------------------------------------

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod v09_3 {
    use super::*;
    use heliosdb_nano::code_graph::CallDirection;
    #[cfg(feature = "mcp-endpoint")]
    use heliosdb_nano::mcp::call_tool;
    use std::collections::{BTreeSet, HashMap, HashSet};

    fn id(v: &Value) -> i64 {
        match v {
            Value::Int4(n) => i64::from(*n),
            Value::Int8(n) => *n,
            other => panic!("not an integer: {other:?}"),
        }
    }

    /// A small corpus: a call chain across three files, a cycle, a shared
    /// name defined in two files, and a leaf that nothing calls.
    fn corpus(db: &EmbeddedDatabase) -> Result<()> {
        insert(
            db,
            "a.py",
            "python",
            "def alpha():\n    return beta()\n\ndef beta():\n    return gamma()\n\ndef shared():\n    return 1\n",
        )?;
        insert(
            db,
            "b.py",
            "python",
            "def gamma():\n    return delta() + alpha()\n\ndef delta():\n    return epsilon()\n\ndef shared():\n    return 2\n",
        )?;
        insert(
            db,
            "c.py",
            "python",
            "def epsilon():\n    return zeta()\n\ndef zeta():\n    return 0\n\ndef lonely():\n    return 3\n",
        )?;
        db.code_index(CodeIndexOptions::for_table("src"))?;
        Ok(())
    }

    #[test]
    fn definition_by_name_with_kind_and_path_hints_unchanged() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        let all = db.lsp_definition("shared", &DefinitionHint::default())?;
        assert_eq!(all.len(), 2);
        let by_path = db.lsp_definition(
            "shared",
            &DefinitionHint {
                hint_file: Some("b.py".into()),
                hint_kind: None,
            },
        )?;
        assert_eq!(by_path.len(), 1);
        assert_eq!(by_path[0].path, "b.py");
        let by_kind = db.lsp_definition(
            "shared",
            &DefinitionHint {
                hint_file: Some("a.py".into()),
                hint_kind: Some("function".into()),
            },
        )?;
        assert_eq!(by_kind.len(), 1);
        assert_eq!(by_kind[0].path, "a.py");
        assert!(db
            .lsp_definition("no_such_symbol", &DefinitionHint::default())?
            .is_empty());
        // The name and path indexes exist and are what the planner is offered.
        let names: BTreeSet<String> = db
            .query("SELECT indexname FROM pg_indexes", &[])?
            .iter()
            .filter_map(|r| match r.values.first() {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        for want in ["idx_hdb_code_symbols_name", "idx_hdb_code_files_path"] {
            assert!(names.contains(want), "missing {want}: {names:?}");
        }
        Ok(())
    }

    #[cfg(feature = "mcp-endpoint")]
    #[test]
    fn document_symbols_same_rows_two_step() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        let call = |args: serde_json::Value| call_tool(Some(&db), "helios_lsp_document_symbols", args);
        let out = call(serde_json::json!({ "path": "b.py" }));
        assert!(!out.is_error, "{:?}", out.payload);
        let got: Vec<(String, i64)> = out.payload["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_string(),
                    s["line_start"].as_i64().unwrap(),
                )
            })
            .collect();
        // Reference: the raw tables, joined in Rust, ordered (line_start, node_id).
        let files: HashMap<i64, String> = db
            .query("SELECT node_id, path FROM _hdb_code_files", &[])?
            .iter()
            .map(|r| {
                (
                    id(&r.values[0]),
                    match &r.values[1] {
                        Value::String(s) => s.clone(),
                        _ => String::new(),
                    },
                )
            })
            .collect();
        let mut reference: Vec<(i64, i64, String)> = db
            .query("SELECT node_id, file_id, name, line_start FROM _hdb_code_symbols", &[])?
            .iter()
            .filter(|r| files.get(&id(&r.values[1])).map(String::as_str) == Some("b.py"))
            .map(|r| {
                (
                    id(&r.values[3]),
                    id(&r.values[0]),
                    match &r.values[2] {
                        Value::String(s) => s.clone(),
                        _ => String::new(),
                    },
                )
            })
            .collect();
        reference.sort();
        let want: Vec<(String, i64)> = reference.into_iter().map(|(l, _, n)| (n, l)).collect();
        assert_eq!(got, want);
        assert!(got.len() >= 3);
        // Kind filter and unknown path.
        let only_fn = call(serde_json::json!({ "path": "b.py", "kinds": ["function"] }));
        assert_eq!(only_fn.payload["symbols"].as_array().unwrap().len(), got.len());
        let none = call(serde_json::json!({ "path": "b.py", "kinds": ["struct"] }));
        assert!(none.payload["symbols"].as_array().unwrap().is_empty());
        let unknown = call(serde_json::json!({ "path": "nope.py" }));
        assert!(!unknown.is_error);
        assert!(unknown.payload["symbols"].as_array().unwrap().is_empty());
        Ok(())
    }

    #[test]
    fn call_hierarchy_in_out_depth3_equals_reference() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        // Reference BFS over the raw tables.
        let refs: Vec<(i64, Option<i64>)> = db
            .query(
                "SELECT from_symbol, to_symbol FROM _hdb_code_symbol_refs WHERE kind = 'CALLS'",
                &[],
            )?
            .iter()
            .map(|r| {
                (
                    id(&r.values[0]),
                    match &r.values[1] {
                        Value::Null => None,
                        v => Some(id(v)),
                    },
                )
            })
            .collect();
        assert!(!refs.is_empty());
        let symbols: Vec<i64> = db
            .query("SELECT node_id FROM _hdb_code_symbols", &[])?
            .iter()
            .map(|r| id(&r.values[0]))
            .collect();
        for dir in [CallDirection::Incoming, CallDirection::Outgoing] {
            for &start in &symbols {
                let mut seen: HashSet<i64> = HashSet::from([start]);
                let mut frontier = vec![start];
                let mut expected: Vec<(i32, BTreeSet<i64>)> = Vec::new();
                for d in 1..=3 {
                    let mut level = BTreeSet::new();
                    for &(f, t) in &refs {
                        let (src, dst) = match dir {
                            CallDirection::Incoming => (t, Some(f)),
                            CallDirection::Outgoing => (Some(f), t),
                        };
                        if let (Some(src), Some(dst)) = (src, dst) {
                            if frontier.contains(&src) && !seen.contains(&dst) {
                                level.insert(dst);
                            }
                        }
                    }
                    seen.extend(level.iter().copied());
                    frontier = level.iter().copied().collect();
                    if !level.is_empty() {
                        expected.push((d, level));
                    }
                }
                let got = db.lsp_call_hierarchy(start, dir, 3)?;
                let mut by_depth: HashMap<i32, BTreeSet<i64>> = HashMap::new();
                for row in &got {
                    assert!(!row.path.is_empty() && !row.qualified.is_empty());
                    assert!(
                        by_depth.entry(row.depth).or_default().insert(row.symbol_id),
                        "duplicate row"
                    );
                }
                let got_levels: Vec<(i32, BTreeSet<i64>)> = {
                    let mut v: Vec<_> = by_depth.into_iter().collect();
                    v.sort_by_key(|(d, _)| *d);
                    v
                };
                assert_eq!(got_levels, expected, "start={start} dir={dir:?}");
            }
        }
        Ok(())
    }

    /// A store bootstrapped by an older build (no name / path index) gains the
    /// indexes at the next `code_index`, and index-served lookups return the
    /// rows a scan does.
    #[test]
    fn old_store_gains_indexes_and_lookups_match_a_scan() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        db.execute("DROP INDEX idx_hdb_code_symbols_name")?;
        db.execute("DROP INDEX idx_hdb_code_files_path")?;
        let scan: BTreeSet<i64> = db
            .query("SELECT node_id, name FROM _hdb_code_symbols", &[])?
            .iter()
            .filter(|r| matches!(&r.values[1], Value::String(n) if n == "shared"))
            .map(|r| id(&r.values[0]))
            .collect();
        insert(&db, "d.py", "python", "def shared():\n    return 4\n")?;
        db.code_index(CodeIndexOptions::for_table("src"))?;
        let names: BTreeSet<String> = db
            .query("SELECT indexname FROM pg_indexes", &[])?
            .iter()
            .filter_map(|r| match r.values.first() {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(names.contains("idx_hdb_code_symbols_name"), "{names:?}");
        assert!(names.contains("idx_hdb_code_files_path"), "{names:?}");
        let served: BTreeSet<i64> = db
            .lsp_definition("shared", &DefinitionHint::default())?
            .iter()
            .map(|d| d.symbol_id)
            .collect();
        assert!(served.is_superset(&scan));
        assert_eq!(served.len(), scan.len() + 1);
        Ok(())
    }
}
