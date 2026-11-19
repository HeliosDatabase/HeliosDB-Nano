//! v10.13 — `heliosdb-nano dump --format sql`.
//!
//! Before: the dump command hard-coded the binary format and had no table
//! filter, and the SQL writer left a partial file behind on error. These drive
//! the command object the CLI builds (`heliosdb_nano::cli::DumpCommand`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::cli::DumpCommand;
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::path::Path;
use tempfile::TempDir;

fn fixture(dir: &Path) {
    let db = EmbeddedDatabase::new(dir).unwrap();
    for sql in [
        "CREATE TABLE customers (id SERIAL PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
        "CREATE TABLE orders (id SERIAL PRIMARY KEY, customer_id INT4 REFERENCES customers(id), total NUMERIC, \
         tags TEXT[], doc JSONB, raw BYTEA, at TIMESTAMP, CHECK (total >= 0))",
        "CREATE INDEX orders_customer ON orders (customer_id)",
        "INSERT INTO customers (name) VALUES ('ann'), ('bob')",
        "INSERT INTO orders (customer_id, total, tags, doc, raw, at) VALUES \
         (1, 10.5, '{a,b}', '{\"k\": 1}', '\\x0102', '2026-01-02 03:04:05'), \
         (2, 3, '{}', '[]', NULL, NULL)",
        "CREATE VIEW order_names AS SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
    ] {
        db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn cmd(data: &Path, out: &Path) -> DumpCommand {
    DumpCommand {
        output: out.to_path_buf(),
        append: false,
        compression: "zstd".into(),
        connection: None,
        verbose: false,
        data_dir: Some(data.to_path_buf()),
        memory: false,
        config: None,
        format: "sql".into(),
        dialect: "nano".into(),
        tables: None,
        schema_only: false,
        data_only: false,
    }
}

fn count(db: &EmbeddedDatabase, table: &str) -> i64 {
    match db.query(&format!("SELECT count(*) FROM {table}"), &[]).unwrap()[0].values[0] {
        Value::Int8(n) => n,
        Value::Int4(n) => i64::from(n),
        ref v => panic!("{v:?}"),
    }
}

fn statements(script: &str) -> Vec<&str> {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .collect()
}

#[test]
fn format_sql_writes_a_restorable_file() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fixture(&data);
    let out = dir.path().join("out.sql");
    cmd(&data, &out).execute().expect("dump --format sql");
    let script = std::fs::read_to_string(&out).unwrap();
    let restored = EmbeddedDatabase::new_in_memory().unwrap();
    restored
        .execute_sql_script(&script)
        .unwrap_or_else(|e| panic!("{e}\n{script}"));
    assert_eq!(count(&restored, "customers"), 2);
    assert_eq!(count(&restored, "orders"), 2);
    assert_eq!(count(&restored, "order_names"), 2);
}

#[test]
fn tables_filter() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fixture(&data);
    let out = dir.path().join("out.sql");
    let mut c = cmd(&data, &out);
    c.tables = Some(vec!["orders".into()]);
    c.execute().unwrap();
    let script = std::fs::read_to_string(&out).unwrap();
    assert!(script.contains("CREATE TABLE IF NOT EXISTS \"orders\""), "{script}");
    assert!(!script.contains("CREATE TABLE IF NOT EXISTS \"customers\""), "{script}");
    assert!(!script.contains("INSERT INTO \"customers\""), "{script}");
    // The FK to a table outside the set is a comment, never a statement.
    for statement in statements(&script) {
        assert!(!statement.contains("FOREIGN KEY"), "{statement}");
    }
    let restored = EmbeddedDatabase::new_in_memory().unwrap();
    restored.execute_sql_script(&script).unwrap();
    assert_eq!(count(&restored, "orders"), 2);

    let mut bad = cmd(&data, &dir.path().join("bad.sql"));
    bad.tables = Some(vec!["nope".into()]);
    assert!(bad.execute().is_err());
}

#[test]
fn schema_only_and_data_only() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fixture(&data);
    let schema = dir.path().join("schema.sql");
    let rows = dir.path().join("rows.sql");
    let mut c = cmd(&data, &schema);
    c.schema_only = true;
    c.execute().unwrap();
    let mut c = cmd(&data, &rows);
    c.data_only = true;
    c.execute().unwrap();
    let schema_text = std::fs::read_to_string(&schema).unwrap();
    let rows_text = std::fs::read_to_string(&rows).unwrap();
    assert!(!schema_text.contains("INSERT INTO"), "{schema_text}");
    assert!(!rows_text.contains("CREATE "), "{rows_text}");

    let restored = EmbeddedDatabase::new_in_memory().unwrap();
    restored.execute_sql_script(&schema_text).unwrap();
    assert_eq!(count(&restored, "orders"), 0);
    restored.execute_sql_script(&rows_text).unwrap();
    assert_eq!(count(&restored, "orders"), 2);

    let mut both = cmd(&data, &dir.path().join("both.sql"));
    both.schema_only = true;
    both.data_only = true;
    assert!(both.execute().is_err());
}

#[test]
fn failed_export_leaves_no_file() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fixture(&data);
    let out = dir.path().join("out.sql");
    std::fs::write(&out, "previous contents").unwrap();
    let mut c = cmd(&data, &out);
    c.tables = Some(vec!["orders".into(), "missing_table".into()]);
    assert!(c.execute().is_err());
    // The target is untouched and no temporary file is left behind.
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "previous contents");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("partial"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn default_dialect_output_is_byte_identical_to_the_library_call() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fixture(&data);
    let out = dir.path().join("cli.sql");
    cmd(&data, &out).execute().unwrap();
    let lib = dir.path().join("lib.sql");
    {
        let db = EmbeddedDatabase::new(&data).unwrap();
        db.dump_sql(&lib).unwrap();
    }
    // The header carries a generation time; everything else is identical.
    let strip = |p: &Path| {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with("-- Generated:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&out), strip(&lib));
}

#[test]
fn sql_only_flags_are_refused_for_binary() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fixture(&data);
    let mut c = cmd(&data, &dir.path().join("x.heliodump"));
    c.format = "binary".into();
    c.tables = Some(vec!["orders".into()]);
    assert!(c.execute().is_err());
    let mut c = cmd(&data, &dir.path().join("x.sql"));
    c.dialect = "oracle".into();
    assert!(c.execute().is_err());
}

#[test]
fn postgres_dialect_is_strict_postgres_text() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fixture(&data);
    {
        let db = EmbeddedDatabase::new(&data).unwrap();
        db.execute("CREATE TABLE docs (id INT PRIMARY KEY, emb VECTOR(3))")
            .unwrap();
        db.execute("CREATE INDEX docs_emb ON docs USING hnsw (emb vector_l2_ops)")
            .unwrap();
        db.execute("CREATE INDEX docs_gin ON docs USING gin (id)").unwrap();
        db.execute("INSERT INTO docs VALUES (1, '[1,2,3]')").unwrap();
    }
    let out = dir.path().join("pg.sql");
    let mut c = cmd(&data, &out);
    c.dialect = "postgres".into();
    c.execute().unwrap();
    let script = std::fs::read_to_string(&out).unwrap();
    let stmts = statements(&script);
    assert_eq!(
        stmts.first().copied(),
        Some("CREATE EXTENSION IF NOT EXISTS vector;"),
        "{script}"
    );
    assert!(script.contains("USING hnsw (\"emb\" vector_l2_ops)"), "{script}");
    assert!(
        script.contains("SELECT setval(pg_get_serial_sequence('\"orders\"', 'id'), COALESCE(MAX(\"id\"), 1), MAX(\"id\") IS NOT NULL) FROM \"orders\";"),
        "{script}"
    );
    // The gin index on a plain column has no portable form: a comment, listed.
    assert!(script.contains("-- NOT PORTABLE:"), "{script}");
    assert!(!stmts.iter().any(|s| s.contains("\"docs_gin\"")), "{script}");
    // Every statement is one line ending in ';' and uses no Nano-only words.
    for s in &stmts {
        assert!(s.ends_with(';'), "{s}");
        for nano_only in ["BRANCH", "AS OF", "STORAGE DICTIONARY", "LOCK-FREE"] {
            assert!(!s.to_uppercase().contains(nano_only), "{s}");
        }
    }
}
