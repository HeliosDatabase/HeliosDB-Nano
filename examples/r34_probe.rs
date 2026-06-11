//! R3.4 probe harness: typed columnar batch format v2 + vectorized kernels.
//!
//! Times the four target shapes on a 500k-row STORAGE COLUMNAR table and the
//! same queries on a row-store twin (regression canary for untouched paths):
//!
//!   A. filtered scalar aggregate   SELECT SUM(v), COUNT(*) WHERE v < c   (~25%)
//!   B. text GROUP BY               SELECT grp, COUNT(*), SUM(v) GROUP BY grp
//!   C. unfiltered SUM              SELECT SUM(v)
//!   D. filter scan                 SELECT v, grp WHERE v >= x AND v < y  (~1%)
//!
//! Run:  cargo run --profile perf --example r34_probe [rows]
//!
//! `v` is decorrelated from row order so zone maps cannot prune (the probe
//! measures kernel speed, not pruning); `w` has NULLs every 31st row.

use heliosdb_nano::{Config, EmbeddedDatabase};
use std::time::Instant;

fn build(db: &EmbeddedDatabase, table: &str, columnar: bool, rows: i64) {
    let storage = if columnar { " STORAGE COLUMNAR" } else { "" };
    db.execute(&format!(
        "CREATE TABLE {table} (id INT8 PRIMARY KEY, v INT8{storage}, w INT8{storage}, grp TEXT{storage})"
    ))
    .unwrap();

    let mut i = 0i64;
    while i < rows {
        let end = (i + 1000).min(rows);
        let mut parts = Vec::with_capacity((end - i) as usize);
        for r in i..end {
            let v = (r * 7919) % 100_000;
            let w = if r % 31 == 0 {
                "NULL".to_string()
            } else {
                ((r * 13) % 5000).to_string()
            };
            parts.push(format!("({r}, {v}, {w}, 'g{}')", r % 24));
        }
        db.execute(&format!(
            "INSERT INTO {table} (id, v, w, grp) VALUES {}",
            parts.join(", ")
        ))
        .unwrap();
        i = end;
    }
}

fn time_query(db: &EmbeddedDatabase, sql: &str) -> (f64, String) {
    // Warm up twice, then report the median of 5 runs.
    let mut digest = String::new();
    for _ in 0..2 {
        let rows = db.query(sql, &[]).unwrap();
        digest = format!("{} rows; first={:?}", rows.len(), rows.first().map(|t| &t.values));
    }
    let mut samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        let rows = db.query(sql, &[]).unwrap();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(rows);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (samples[2], digest)
}

fn main() {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000);

    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.wal_enabled = false;
    let db = EmbeddedDatabase::with_config(config).unwrap();

    let load_start = Instant::now();
    build(&db, "probe_c", true, rows);
    let columnar_load = load_start.elapsed().as_secs_f64();
    let load_start = Instant::now();
    build(&db, "probe_r", false, rows);
    let row_load = load_start.elapsed().as_secs_f64();
    println!("load: columnar {columnar_load:.2}s, row {row_load:.2}s ({rows} rows each)");

    let shapes: [(&str, &str); 4] = [
        ("A filtered-agg ", "SELECT SUM(v), COUNT(*) FROM {t} WHERE v < 25000"),
        ("B text-group-by", "SELECT grp, COUNT(*), SUM(v) FROM {t} GROUP BY grp"),
        ("C unfiltered-sum", "SELECT SUM(v) FROM {t}"),
        ("D filter-scan  ", "SELECT v, grp FROM {t} WHERE v >= 42000 AND v < 43000"),
    ];

    for (name, template) in shapes {
        let (col_ms, col_digest) = time_query(&db, &template.replace("{t}", "probe_c"));
        let (row_ms, _) = time_query(&db, &template.replace("{t}", "probe_r"));
        println!("{name}  columnar {col_ms:9.3} ms   row-store {row_ms:9.3} ms   [{col_digest}]");
    }
}
