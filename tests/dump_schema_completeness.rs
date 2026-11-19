//! v10.12 — a dump restores the whole schema.
//!
//! Before: neither the SQL nor the binary dump carried sequences, SERIAL /
//! IDENTITY columns, ordinary or UNIQUE indexes, expression indexes or views,
//! and a vector index lost its distance metric. After a restore an INSERT
//! without its key failed, a duplicate the unique index used to reject was
//! accepted, the views were gone and `nextval` restarted.
//!
//! Every case runs for BOTH formats: `dump_sql` + `execute_sql_script`, and
//! `dump_full` + `restore_from_dump`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, Value};
use tempfile::TempDir;

#[derive(Clone, Copy, Debug)]
enum Format {
    Sql,
    Binary,
}

const FORMATS: [Format; 2] = [Format::Sql, Format::Binary];

const FIXTURE: &[&str] = &[
    "CREATE SEQUENCE order_no START WITH 100",
    "CREATE TABLE orders (id SERIAL PRIMARY KEY, order_no INT8, customer TEXT, amount NUMERIC(12,2))",
    "CREATE UNIQUE INDEX orders_order_no_uq ON orders (order_no)",
    "CREATE INDEX orders_customer_idx ON orders (customer)",
    "CREATE INDEX orders_lower_customer ON orders ((lower(customer)))",
    "CREATE TABLE events (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, note TEXT)",
    "CREATE TABLE docs (id INT PRIMARY KEY, emb VECTOR(3))",
    "CREATE INDEX docs_emb ON docs USING hnsw (emb vector_cosine_ops)",
    "INSERT INTO orders (order_no, customer, amount) VALUES (nextval('order_no'), 'ann', 12.50)",
    "INSERT INTO orders (order_no, customer, amount) VALUES (nextval('order_no'), 'bob', 7.25)",
    "INSERT INTO orders (order_no, customer, amount) VALUES (nextval('order_no'), 'cid', 99.99)",
    "INSERT INTO events (note) VALUES ('a')",
    "INSERT INTO events (note) VALUES ('b')",
    "INSERT INTO docs VALUES (1, '[1,0,0]'), (2, '[0,1,0]')",
    "CREATE VIEW big_orders AS SELECT id, customer FROM orders WHERE amount > 10",
];

struct Restored {
    db: EmbeddedDatabase,
    _dir: TempDir,
}

fn source() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("source");
    for statement in FIXTURE {
        db.execute(statement)
            .unwrap_or_else(|e| panic!("fixture failed: {statement}\n  {e}"));
    }
    db
}

fn round_trip(format: Format) -> Restored {
    let src = source();
    let dir = TempDir::new().expect("tempdir");
    let mut db = EmbeddedDatabase::new_in_memory().expect("target");
    match format {
        Format::Sql => {
            let path = dir.path().join("dump.sql");
            src.dump_sql(&path).expect("dump_sql");
            let script = std::fs::read_to_string(&path).expect("read");
            db.execute_sql_script(&script)
                .unwrap_or_else(|e| panic!("sql restore failed: {e}\n----\n{script}"));
        }
        Format::Binary => {
            let path = dir.path().join("dump.heliodump");
            src.dump_full(&path).expect("dump_full");
            db.restore_from_dump(&path).expect("binary restore");
        }
    }
    Restored { db, _dir: dir }
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> Value {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.first()
        .and_then(|r| r.values.first().cloned())
        .unwrap_or_else(|| panic!("{sql}: no rows"))
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => format!("{other}"),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

#[test]
fn unique_index_survives_and_rejects_a_duplicate() {
    for format in FORMATS {
        let r = round_trip(format);
        let err =
            r.db.execute("INSERT INTO orders (order_no, customer) VALUES (101, 'dup')")
                .expect_err("a duplicate order_no must be rejected after restore");
        assert!(
            err.to_string().to_lowercase().contains("unique") || err.to_string().contains("duplicate"),
            "{format:?}: {err}"
        );
        // Through the parameter family as well.
        let err =
            r.db.execute_params(
                "INSERT INTO orders (order_no, customer) VALUES ($1, $2)",
                &[Value::Int8(102), Value::String("dup".into())],
            )
            .expect_err("duplicate through execute_params");
        assert!(!err.to_string().is_empty(), "{format:?}");
    }
}

#[test]
fn ordinary_index_survives() {
    for format in FORMATS {
        let r = round_trip(format);
        let rows =
            r.db.query(
                "SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'orders'",
                &[],
            )
            .unwrap();
        let defs: Vec<String> = rows.iter().map(|t| text(&t.values[1])).collect();
        let names: Vec<String> = rows.iter().map(|t| text(&t.values[0])).collect();
        assert!(
            names.iter().any(|n| n == "orders_customer_idx"),
            "{format:?}: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "orders_lower_customer"),
            "{format:?}: expression index missing: {names:?} {defs:?}"
        );
    }
}

#[test]
fn sequence_continues_after_its_last_value() {
    for format in FORMATS {
        let r = round_trip(format);
        let next = as_i64(&scalar(&r.db, "SELECT nextval('order_no')"));
        assert_eq!(next, 103, "{format:?}: nextval must continue after 102");
    }
}

#[test]
fn serial_column_generates_after_restore() {
    for format in FORMATS {
        let r = round_trip(format);
        r.db.execute("INSERT INTO orders (order_no, customer) VALUES (500, 'new')")
            .unwrap_or_else(|e| panic!("{format:?}: insert without id failed: {e}"));
        let id = as_i64(&scalar(&r.db, "SELECT id FROM orders WHERE order_no = 500"));
        assert!(id > 3, "{format:?}: generated id {id} collides with restored rows");
        let count = as_i64(&scalar(&r.db, "SELECT count(*) FROM orders"));
        assert_eq!(count, 4, "{format:?}");
    }
}

#[test]
fn identity_always_column_survives() {
    for format in FORMATS {
        let r = round_trip(format);
        r.db.execute_params("INSERT INTO events (note) VALUES ($1)", &[Value::String("c".into())])
            .unwrap_or_else(|e| panic!("{format:?}: insert without identity failed: {e}"));
        let id = as_i64(&scalar(&r.db, "SELECT id FROM events WHERE note = 'c'"));
        assert!(id > 2, "{format:?}: identity restarted at {id}");
    }
}

#[test]
fn view_survives_and_returns_rows() {
    for format in FORMATS {
        let r = round_trip(format);
        let n = as_i64(&scalar(&r.db, "SELECT count(*) FROM big_orders"));
        assert_eq!(n, 2, "{format:?}");
    }
}

#[test]
fn hnsw_metric_survives() {
    for format in FORMATS {
        let r = round_trip(format);
        let def = scalar(&r.db, "SELECT indexdef FROM pg_indexes WHERE indexname = 'docs_emb'");
        assert!(format!("{def}").contains("vector_cosine_ops"), "{format:?}: {def}");
    }
}

#[test]
fn sql_dump_lists_objects_in_postgres_spelling() {
    let src = source();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("d.sql");
    src.dump_sql(&path).unwrap();
    let script = std::fs::read_to_string(&path).unwrap();
    assert!(
        script.contains("CREATE SEQUENCE IF NOT EXISTS \"order_no\""),
        "{script}"
    );
    assert!(script.contains("SELECT setval('order_no', 102, true);"), "{script}");
    assert!(script.contains("\"id\" SERIAL PRIMARY KEY"), "{script}");
    assert!(script.contains("\"id\" BIGSERIAL PRIMARY KEY"), "{script}");
    assert!(
        script.contains("CREATE UNIQUE INDEX \"orders_order_no_uq\""),
        "{script}"
    );
    assert!(script.contains("CREATE VIEW \"big_orders\" AS SELECT"), "{script}");
    // Views come last, after the indexes, which come after the data.
    let insert = script.find("INSERT INTO \"orders\"").unwrap();
    let index = script.find("CREATE UNIQUE INDEX").unwrap();
    let view = script.find("CREATE VIEW").unwrap();
    assert!(insert < index && index < view, "{script}");
}

#[test]
fn dump_from_previous_format_still_restores() {
    // A v2 file is a v3 file with no SCHM section: same TABL layout.
    let src = EmbeddedDatabase::new_in_memory().unwrap();
    src.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    src.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("v2.heliodump");
    src.dump_full(&path).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]), 3);
    bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let mut db = EmbeddedDatabase::new_in_memory().unwrap();
    db.restore_from_dump(&path).expect("v2 dump restores");
    assert_eq!(as_i64(&scalar(&db, "SELECT count(*) FROM t")), 2);
}

#[test]
fn a_dump_of_a_unique_violation_is_refused_on_restore() {
    // Restore runs CREATE UNIQUE INDEX through the normal path, which
    // validates the restored rows: hand-edit a SQL dump so the data violates
    // its own unique index and the load must fail on that statement.
    let src = source();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("d.sql");
    src.dump_sql(&path).unwrap();
    let script = std::fs::read_to_string(&path).unwrap().replace("101", "100");
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let err = db
        .execute_sql_script(&script)
        .expect_err("duplicate rows under a unique index");
    assert!(
        err.to_string().contains("orders_order_no_uq") || err.to_string().to_lowercase().contains("unique"),
        "{err}"
    );
}
