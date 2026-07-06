//! Item #3 increment-0 go/no-go measurement (NOT a correctness test — ignored):
//!
//!   cargo test --release --test columnar_increment0_bench -- --ignored --nocapture --test-threads=1
//!
//! Loads 1M rows into a STORAGE COLUMNAR table AND a row-store twin, then times
//! the analytics queries the columnar engine is meant to accelerate on each.
//! GATE: if the already-shipped vectorized kernels don't deliver >= 5x over the
//! row-store twin on the analytical queries, STOP item #3 (its premise fails).
//! Documented SQLite baselines on a comparable 1M-row table (PROPOSAL_COLUMNAR_
//! STORAGE.md): COUNT(DISTINCT) 27 ms, WHERE filter 76 ms, GROUP BY SUM 145 ms.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod inc0 {
    use heliosdb_nano::{Config, EmbeddedDatabase, Value};
    use std::time::Instant;

    fn test_db() -> EmbeddedDatabase {
        let mut config = Config::default();
        config.storage.memory_only = true;
        config.storage.wal_enabled = false;
        EmbeddedDatabase::with_config(config).expect("db")
    }

    // id PK, v (measure/agg col), g (10 groups), s (session-ish, high card for
    // COUNT DISTINCT), w (nullable filter col).
    fn bulk_insert(db: &EmbeddedDatabase, table: &str, rows: i64, per_batch: i64) -> f64 {
        let sql = format!("INSERT INTO {table} (id, v, g, s, w) VALUES ($1, $2, $3, $4, $5)");
        let start = Instant::now();
        let mut id = 0i64;
        while id < rows {
            let end = (id + per_batch).min(rows);
            let batch: Vec<Vec<Value>> = (id..end)
                .map(|i| {
                    vec![
                        Value::Int8(i),
                        Value::Int8((i * 7) % 100_000),
                        Value::Int8(i % 10),
                        Value::Int8(i % 50_000),
                        if i % 17 == 0 { Value::Null } else { Value::Int8(i % 4000) },
                    ]
                })
                .collect();
            db.execute_many_params(&sql, &batch).unwrap();
            id = end;
        }
        start.elapsed().as_secs_f64()
    }

    fn time_query(db: &EmbeddedDatabase, sql: &str, reps: usize) -> f64 {
        let _ = db.query(sql, &[]).unwrap(); // warm
        let mut best = f64::MAX;
        for rep in 0..reps {
            let sql_rep = format!("{sql} /* rep {rep} */"); // dodge result cache
            let start = Instant::now();
            let _ = db.query(&sql_rep, &[]).unwrap();
            best = best.min(start.elapsed().as_secs_f64());
        }
        best
    }

    #[test]
    #[ignore]
    fn increment0_columnar_vs_row_1m() {
        const N: i64 = 1_000_000;
        let col = test_db();
        col.execute(
            "CREATE TABLE t (id INT8 PRIMARY KEY, v INT8 STORAGE COLUMNAR, \
             g INT8 STORAGE COLUMNAR, s INT8 STORAGE COLUMNAR, w INT8 STORAGE COLUMNAR)",
        )
        .unwrap();
        let cs = bulk_insert(&col, "t", N, 2_000);

        let row = test_db();
        row.execute("CREATE TABLE t (id INT8 PRIMARY KEY, v INT8, g INT8, s INT8, w INT8)")
            .unwrap();
        let rs = bulk_insert(&row, "t", N, 2_000);

        println!("\n=== Item #3 increment-0: columnar vs row-store, {N} rows ===");
        println!("load: columnar {cs:.2}s  row {rs:.2}s\n");
        println!("{:<42} {:>10} {:>10} {:>8}", "query", "col(ms)", "row(ms)", "speedup");
        println!("{}", "-".repeat(74));

        let queries = [
            ("unfiltered SUM/MIN/MAX", "SELECT SUM(v), MIN(v), MAX(v) FROM t"),
            ("WHERE v<1000 COUNT/SUM", "SELECT COUNT(*), SUM(v) FROM t WHERE v < 1000"),
            ("GROUP BY g SUM", "SELECT g, COUNT(*), SUM(v) FROM t GROUP BY g"),
            ("COUNT(DISTINCT s)", "SELECT COUNT(DISTINCT s) FROM t"),
            ("WHERE w IS NULL GROUP BY g", "SELECT g, COUNT(*), SUM(v) FROM t WHERE w IS NULL GROUP BY g"),
        ];
        let mut speedups = Vec::new();
        for (name, sql) in queries {
            let c = time_query(&col, sql, 5) * 1e3;
            let r = time_query(&row, sql, 5) * 1e3;
            let sp = r / c;
            speedups.push((name, sp));
            println!("{name:<42} {c:>10.1} {r:>10.1} {sp:>7.1}x");
        }
        println!("{}", "-".repeat(74));
        let best = speedups.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
        let median = {
            let mut v: Vec<f64> = speedups.iter().map(|(_, s)| *s).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        println!("VERDICT: best {best:.1}x, median {median:.1}x vs row-store.");
        println!(
            "  {} (gate: any analytical query >= 5x vs row-store => item #3 proceeds)",
            if best >= 5.0 { "PROCEED" } else { "STOP - shipped kernels below 5x" }
        );
        println!("  (SQLite ref @1M: COUNT DISTINCT 27ms, filter 76ms, GROUP BY SUM 145ms)\n");
    }
}
