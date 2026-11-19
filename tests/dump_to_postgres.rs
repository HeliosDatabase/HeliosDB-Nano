//! v10.13 — `dump --format sql --dialect postgres` loads into a real
//! PostgreSQL with zero errors, and the loaded rows equal Nano's.
//!
//! Needs a PostgreSQL reference and a `psql` to reach it, so it is ignored by
//! default. Run it with:
//!
//! ```text
//! NANO_PSQL='psql -h 127.0.0.1 -p 25433 -U bench -w' \
//!   cargo test --test dump_to_postgres -- --ignored
//! ```
//!
//! `NANO_PSQL` is a shell command prefix (a `docker run … psql` wrapper works);
//! the test creates and drops its own scratch database. The fixture has serial
//! keys, a composite key, a foreign key, CHECKs, ordinary and unique indexes,
//! a view, JSONB, arrays, numeric, bytea and timestamps. A VECTOR column is
//! covered by the strict text check in `tests/dump_cli_sql.rs` instead (the
//! reference has no pgvector).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::cli::DumpCommand;
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::process::Command;

const FIXTURE: &[&str] = &[
    "CREATE TABLE customers (id SERIAL PRIMARY KEY, name TEXT NOT NULL, email TEXT, CHECK (length(name) > 0))",
    "CREATE UNIQUE INDEX customers_email_uq ON customers (email)",
    "CREATE TABLE orders (id BIGSERIAL PRIMARY KEY, customer_id INT4 REFERENCES customers(id), \
     total NUMERIC, tags TEXT[], doc JSONB, raw BYTEA, at TIMESTAMP, CHECK (total >= 0))",
    "CREATE INDEX orders_customer ON orders (customer_id)",
    "CREATE TABLE lines (order_id INT8, line_no INT4, qty INT4, PRIMARY KEY (order_id, line_no))",
    "INSERT INTO customers (name, email) VALUES ('ann', 'a@x'), ('bob', 'b@x'), ('o''neil', NULL)",
    "INSERT INTO orders (customer_id, total, tags, doc, raw, at) VALUES \
     (1, 10.5, '{a,b}', '{\"k\": 1}', '\\x0102', '2026-01-02 03:04:05'), \
     (2, 3, '{}', '[]', NULL, NULL), (1, 0.25, '{\"x y\"}', '{\"n\": null}', '\\xff', '2025-12-31 23:59:59')",
    "INSERT INTO lines VALUES (1, 1, 2), (1, 2, 5), (2, 1, 1)",
    "CREATE VIEW order_names AS SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id",
];

/// Comparable text per table: columns whose text form is the same in both.
const CHECKS: &[(&str, &str)] = &[
    ("customers", "SELECT id, name, email FROM customers ORDER BY id"),
    ("orders", "SELECT id, customer_id, total FROM orders ORDER BY id"),
    (
        "lines",
        "SELECT order_id, line_no, qty FROM lines ORDER BY order_id, line_no",
    ),
    ("order_names", "SELECT id, name FROM order_names ORDER BY id"),
];

fn psql(prefix: &str, db: &str, args: &str) -> (bool, String) {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{prefix} -d {db} -v ON_ERROR_STOP=1 -At {args}"))
        .output()
        .expect("run psql");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn nano_rows(db: &EmbeddedDatabase, sql: &str) -> String {
    db.query(sql, &[])
        .unwrap()
        .iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::String(text) => text.clone(),
                    other => format!("{other}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[ignore = "needs the PostgreSQL reference: set NANO_PSQL and run with --ignored"]
fn postgres_dialect_dump_loads_into_postgresql_with_zero_errors() {
    let prefix = std::env::var("NANO_PSQL").expect("NANO_PSQL");
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let db = EmbeddedDatabase::new(&data).unwrap();
        for sql in FIXTURE {
            db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    }
    // Written where the psql command can read it (a docker wrapper mounts
    // NANO_PG_DUMP_DIR); defaults to the tempdir.
    let out_dir = std::env::var("NANO_PG_DUMP_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dir.path().to_path_buf());
    let out = out_dir.join("nano_pg_dump.sql");
    DumpCommand {
        output: out.clone(),
        append: false,
        compression: "zstd".into(),
        connection: None,
        verbose: false,
        data_dir: Some(data.clone()),
        memory: false,
        config: None,
        format: "sql".into(),
        dialect: "postgres".into(),
        tables: None,
        schema_only: false,
        data_only: false,
    }
    .execute()
    .unwrap();

    let scratch = format!("nano_dump_check_{}", std::process::id());
    let (ok, text) = psql(&prefix, "postgres", &format!("-c 'CREATE DATABASE {scratch}'"));
    assert!(ok, "{text}");
    let file_arg = std::env::var("NANO_PG_DUMP_PATH_IN_PSQL").unwrap_or_else(|_| out.display().to_string());
    let (loaded, load_text) = psql(&prefix, &scratch, &format!("-f {file_arg}"));
    let mut mismatches = Vec::new();
    if loaded {
        let db = EmbeddedDatabase::new(&data).unwrap();
        for (name, sql) in CHECKS {
            let (ok, pg) = psql(&prefix, &scratch, &format!("-c \"{sql}\""));
            let nano = nano_rows(&db, sql);
            if !ok || pg.trim_end() != nano.trim_end() {
                mismatches.push(format!("{name}:\n  postgres: {pg}\n  nano:     {nano}"));
            }
        }
        // A SERIAL key generated after the load continues past the loaded ones.
        let (ok, next) = psql(
            &prefix,
            &scratch,
            "-c \"INSERT INTO customers (name) VALUES ('new') RETURNING id\"",
        );
        if !ok || next.lines().next() != Some("4") {
            mismatches.push(format!("serial after load: {next}"));
        }
    }
    let _ = psql(&prefix, "postgres", &format!("-c 'DROP DATABASE {scratch}'"));
    assert!(loaded, "psql -v ON_ERROR_STOP=1 failed:\n{load_text}");
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
