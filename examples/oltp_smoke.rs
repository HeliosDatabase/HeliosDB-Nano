//! Standalone OLTP smoke benchmark for the `feat/predicate-pushdown` branch.
//!
//! Mirrors the key workloads from `benches/external/pg_vs_helios.py` (which
//! goes through the PG wire) but uses the embedded API directly so we can
//! attribute timing differences to in-process changes only.
//!
//! Run:
//! ```bash
//! cargo run --release --example oltp_smoke
//! ```
//!
//! Compare numbers to `docs/BENCHMARK_PG_VS_HELIOS.txt`. Differences vs.
//! the documented PG-wire baseline reflect (PG wire overhead) +
//! (in-process changes since that benchmark ran). For this branch in
//! particular we want to confirm the JoinPredicatePushdownRule does not
//! regress single-row INSERT, batch INSERT, repeated queries, or any
//! read path.

use std::time::{Duration, Instant};

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

const BATCH_ROWS: usize = 1000;
const REPEATED_RUNS: usize = 100;
const NUM_MAIN_ROWS: usize = 10_000;
const NUM_ORDER_ROWS: usize = 5_000;

fn time_ms<F: FnOnce() -> Result<()>>(f: F) -> f64 {
    let start = Instant::now();
    f().expect("op succeeded");
    elapsed_ms(start)
}

fn elapsed_ms(start: Instant) -> f64 {
    let d: Duration = start.elapsed();
    (d.as_secs_f64()) * 1000.0
}

fn report(label: &str, ms: f64, n: usize) {
    let rate = if ms > 0.0 { n as f64 / (ms / 1000.0) } else { 0.0 };
    println!(
        "  {:<40} {:>10.2} ms  ({:>10.0} ops/s)",
        label, ms, rate
    );
}

fn setup(db: &EmbeddedDatabase) -> Result<()> {
    db.execute(
        "CREATE TABLE bench_main (
            id INT PRIMARY KEY,
            name TEXT,
            age INT,
            score DOUBLE PRECISION,
            active BOOLEAN,
            category TEXT
        )",
    )?;
    db.execute(
        "CREATE TABLE bench_orders (
            id INT PRIMARY KEY,
            user_id INT,
            amount DOUBLE PRECISION,
            status TEXT
        )",
    )?;
    db.execute(
        "CREATE TABLE bench_batch (
            id INT PRIMARY KEY,
            name TEXT,
            age INT,
            score DOUBLE PRECISION,
            active BOOLEAN,
            category TEXT
        )",
    )?;

    // Bulk-load main + orders for the read-heavy workload.
    db.execute("BEGIN")?;
    for i in 0..NUM_MAIN_ROWS {
        db.execute_params(
            "INSERT INTO bench_main (id, name, age, score, active, category) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                Value::Int4(i as i32),
                Value::String(format!("user_{}", i)),
                Value::Int4(20 + (i % 50) as i32),
                Value::Float8(i as f64 * 0.1),
                Value::Boolean(i % 2 == 0),
                Value::String(format!("cat_{}", i % 10)),
            ],
        )?;
    }
    db.execute("COMMIT")?;

    db.execute("BEGIN")?;
    for i in 0..NUM_ORDER_ROWS {
        db.execute_params(
            "INSERT INTO bench_orders (id, user_id, amount, status) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int4(i as i32),
                Value::Int4((i % NUM_MAIN_ROWS) as i32),
                Value::Float8(i as f64 * 1.5),
                Value::String("ok".into()),
            ],
        )?;
    }
    db.execute("COMMIT")?;
    Ok(())
}

fn bench_batch_insert(db: &EmbeddedDatabase) {
    let runs = 3;
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        db.execute("DELETE FROM bench_batch").unwrap();
        let ms = time_ms(|| {
            db.execute("BEGIN")?;
            for i in 0..BATCH_ROWS {
                db.execute_params(
                    "INSERT INTO bench_batch (id, name, age, score, active, category) \
                     VALUES ($1, $2, $3, $4, $5, $6)",
                    &[
                        Value::Int4(i as i32),
                        Value::String(format!("batch_{}", i)),
                        Value::Int4(20 + (i % 50) as i32),
                        Value::Float8(i as f64 * 0.1),
                        Value::Boolean(i % 2 == 0),
                        Value::String("batch".into()),
                    ],
                )?;
            }
            db.execute("COMMIT")?;
            Ok(())
        });
        times.push(ms);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = times[runs / 2];
    report("Batch INSERT (1000 rows)", median, BATCH_ROWS);
}

fn bench_single_insert(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE bench_single (id INT PRIMARY KEY, val INT)")
        .unwrap();
    let runs = 100;
    let mut times = Vec::with_capacity(runs);
    for i in 0..runs {
        let ms = time_ms(|| {
            db.execute_params(
                "INSERT INTO bench_single (id, val) VALUES ($1, $2)",
                &[Value::Int4(i as i32), Value::Int4(i as i32 * 2)],
            )?;
            Ok(())
        });
        times.push(ms);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = times[runs / 2];
    report("INSERT single + commit (median)", median, 1);
}

fn bench_pk_lookup_hot(db: &EmbeddedDatabase) {
    // Warmup
    let _ = db.query("SELECT * FROM bench_main WHERE id = 5000", &[]).unwrap();
    let runs = 100;
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let ms = time_ms(|| {
            let _ = db.query("SELECT * FROM bench_main WHERE id = 5000", &[])?;
            Ok(())
        });
        times.push(ms);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = times[runs / 2];
    report("PK lookup (hot, median of 100)", median, 1);
}

fn bench_count_star(db: &EmbeddedDatabase) {
    let runs = 5;
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let ms = time_ms(|| {
            let _ = db.query("SELECT COUNT(*) FROM bench_main", &[])?;
            Ok(())
        });
        times.push(ms);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report("COUNT(*) (median of 5)", times[runs / 2], 1);
}

fn bench_inner_join(db: &EmbeddedDatabase) {
    // Warmup so plan + result caches are primed.
    for _ in 0..10 {
        let _ = db
            .query(
                "SELECT bench_main.name, bench_orders.amount \
                   FROM bench_main JOIN bench_orders ON bench_main.id = bench_orders.user_id \
                  LIMIT 100",
                &[],
            )
            .unwrap();
    }
    let runs = 2000;
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let ms = time_ms(|| {
            let _ = db.query(
                "SELECT bench_main.name, bench_orders.amount \
                   FROM bench_main JOIN bench_orders ON bench_main.id = bench_orders.user_id \
                  LIMIT 100",
                &[],
            )?;
            Ok(())
        });
        times.push(ms);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = times[runs / 2];
    let p99 = times[(runs * 99) / 100];
    let mean = times.iter().sum::<f64>() / runs as f64;
    println!(
        "  {:<40} p50={:>7.4} ms  mean={:>7.4} ms  p99={:>7.4} ms  (n={})",
        "INNER JOIN", p50, mean, p99, runs
    );
}

fn bench_repeated_query(db: &EmbeddedDatabase) {
    // Warmup: prime the plan/result cache
    for _ in 0..5 {
        let _ = db.query("SELECT * FROM bench_main WHERE id = 1", &[]).unwrap();
    }
    let start = Instant::now();
    for _ in 0..REPEATED_RUNS {
        let _ = db
            .query("SELECT * FROM bench_main WHERE id = 1", &[])
            .unwrap();
    }
    let total = elapsed_ms(start);
    report(&format!("Repeated query x{} (cached)", REPEATED_RUNS), total, REPEATED_RUNS);
}

fn main() -> Result<()> {
    println!("OLTP smoke bench — feat/predicate-pushdown");
    println!("============================================");
    println!("(Embedded API; mirrors pg_vs_helios.py shapes)");
    println!();
    let setup_path = std::env::temp_dir().join("oltp-smoke-bench-data");
    if setup_path.exists() {
        std::fs::remove_dir_all(&setup_path).ok();
    }

    let db = EmbeddedDatabase::new(&setup_path)?;
    let setup_ms = time_ms(|| setup(&db));
    println!("Setup: {:.0} ms ({} main + {} orders rows seeded)", setup_ms, NUM_MAIN_ROWS, NUM_ORDER_ROWS);
    println!();

    println!("WRITE PERFORMANCE");
    println!("─────────────────");
    bench_batch_insert(&db);
    bench_single_insert(&db);
    println!();

    println!("READ PERFORMANCE");
    println!("────────────────");
    bench_pk_lookup_hot(&db);
    bench_count_star(&db);
    bench_inner_join(&db);
    bench_repeated_query(&db);
    println!();

    println!("Done. Compare to docs/BENCHMARK_PG_VS_HELIOS.txt for historical numbers.");
    Ok(())
}
