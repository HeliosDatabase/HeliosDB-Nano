//! R3.1 zone-map pruning A/B benchmark (focused test print, columnar_analytics
//! pattern). Skipped unless HELIOS_ZONE_BENCH=1.
//!
//!   HELIOS_ZONE_BENCH=1 cargo test --release --test zone_map_bench -- --nocapture --test-threads=1
//!
//! A/B:
//!   default               -> zone-map pruning on (after)
//!   HELIOS_ZONE_MAP_OFF=1 -> pre-R3.1 eager read path (before)
//! and the same file dropped into a pristine main worktree gives the true
//! "before" baseline.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod zone_map_bench {
    use heliosdb_nano::{EmbeddedDatabase, Value};
    use std::time::Instant;

    fn to_i64(v: &Value) -> i64 {
        match v {
            Value::Int2(n) => *n as i64,
            Value::Int4(n) => *n as i64,
            Value::Int8(n) => *n,
            Value::Null => 0,
            other => panic!("expected integer, got {other:?}"),
        }
    }

    #[test]
    fn run_zone_map_ab_bench() {
        if std::env::var("HELIOS_ZONE_BENCH").is_err() {
            eprintln!("skipping run_zone_map_ab_bench (set HELIOS_ZONE_BENCH=1)");
            return;
        }
        let n: i64 = std::env::var("HELIOS_ZONE_BENCH_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(500_000);
        let runs = 5usize;
        let pruning = if std::env::var("HELIOS_ZONE_MAP_OFF").is_ok() {
            "OFF (kill switch)"
        } else {
            "ON"
        };

        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(
            "CREATE TABLE bench (
                id INTEGER PRIMARY KEY,
                v INTEGER STORAGE COLUMNAR,
                w INTEGER STORAGE COLUMNAR
            )",
        )
        .unwrap();

        let t = Instant::now();
        db.execute("BEGIN").unwrap();
        for i in 0..n {
            db.execute(&format!(
                "INSERT INTO bench (id, v, w) VALUES ({i}, {i}, {})",
                (i * 13) % 10_000
            ))
            .unwrap();
        }
        db.execute("COMMIT").unwrap();
        let insert_secs = t.elapsed().as_secs_f64();

        println!("\n=== zone-map A/B bench: N={n}  pruning={pruning} ===");
        println!(
            "insert (1 txn)            {insert_secs:>9.2} s   {:>10.0} rows/s",
            n as f64 / insert_secs
        );

        let expect_count = |lo: i64, hi: i64| -> i64 { (lo.max(0)..=hi.min(n - 1)).count() as i64 };
        // Trailing-space padding makes every run's SQL text unique so the
        // R2.1 result/plan caches cannot serve repeats (same trick as
        // run_columnar_scan_bench's agg_no_filter).
        let mut bench = |label: &str, sql: &str, expected_count: i64| {
            // warm-up + correctness check
            let rows = db.query(&format!("{sql}{}", " ".repeat(runs + 1)), &[]).unwrap();
            assert_eq!(to_i64(&rows[0].values[0]), expected_count, "{label} COUNT mismatch");
            let t = Instant::now();
            for r in 0..runs {
                let rows = db.query(&format!("{sql}{}", " ".repeat(r)), &[]).unwrap();
                assert_eq!(to_i64(&rows[0].values[0]), expected_count);
            }
            let us = t.elapsed().as_secs_f64() * 1e6 / runs as f64;
            println!("{label:<26} {us:>10.1} us/query  ({expected_count} rows matched)");
        };

        let lo_2pct = n - n / 50;
        bench(
            "agg_2pct_clustered",
            &format!("SELECT COUNT(*), SUM(w) FROM bench WHERE v >= {lo_2pct}"),
            expect_count(lo_2pct, n - 1),
        );
        let lo_mid = n / 2;
        let hi_mid = n / 2 + n / 100 - 1;
        bench(
            "agg_1pct_between_mid",
            &format!("SELECT COUNT(*), SUM(w) FROM bench WHERE v BETWEEN {lo_mid} AND {hi_mid}"),
            expect_count(lo_mid, hi_mid),
        );
        bench(
            "agg_0pct_no_match",
            &format!("SELECT COUNT(*), SUM(w) FROM bench WHERE v >= {}", n * 2),
            0,
        );
        bench(
            "agg_eq_point",
            &format!("SELECT COUNT(*), SUM(w) FROM bench WHERE v = {}", n / 3),
            1,
        );
        bench(
            "agg_100pct_regression",
            "SELECT COUNT(*), SUM(w) FROM bench WHERE v >= 0",
            n,
        );
        println!();
    }
}
