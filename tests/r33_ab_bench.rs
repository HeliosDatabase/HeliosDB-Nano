//! R3.3 A/B benchmark (not a correctness test — run explicitly):
//!
//!   cargo test --release --test r33_ab_bench -- --ignored --nocapture --test-threads=1
//!
//! Drop this same file into a pre-R3.3 worktree for the BEFORE numbers.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod r33_ab {
    use heliosdb_nano::{Config, EmbeddedDatabase, Value};
    use std::time::Instant;

    fn test_db() -> EmbeddedDatabase {
        let mut config = Config::default();
        config.storage.memory_only = true;
        config.storage.wal_enabled = false;
        EmbeddedDatabase::with_config(config).expect("Failed to create database")
    }

    fn bulk_insert(db: &EmbeddedDatabase, table: &str, rows: i64, per_batch: i64) -> f64 {
        let sql = format!("INSERT INTO {table} (id, v, g, w) VALUES ($1, $2, $3, $4)");
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
                        if i % 17 == 0 {
                            Value::Null
                        } else {
                            Value::Int8(i % 4000)
                        },
                    ]
                })
                .collect();
            db.execute_many_params(&sql, &batch).unwrap();
            id = end;
        }
        start.elapsed().as_secs_f64()
    }

    fn time_query(db: &EmbeddedDatabase, sql: &str, reps: usize) -> (f64, usize) {
        // Warm once.
        let rows = db.query(sql, &[]).unwrap().len();
        let mut best = f64::MAX;
        for rep in 0..reps {
            // Unique comment per rep: bypasses the result cache so the
            // execution path itself is measured.
            let sql_rep = format!("{sql} /* rep {rep} */");
            let start = Instant::now();
            let r = db.query(&sql_rep, &[]).unwrap();
            assert_eq!(r.len(), rows);
            best = best.min(start.elapsed().as_secs_f64());
        }
        (best, rows)
    }

    #[test]
    #[ignore]
    fn ab_bulk_insert_200k_columnar() {
        let db = test_db();
        db.execute("CREATE TABLE m (id INT8 PRIMARY KEY, v INT8 STORAGE COLUMNAR, g INT8 STORAGE COLUMNAR, w INT8)")
            .unwrap();
        let secs = bulk_insert(&db, "m", 200_000, 1_000);
        let count = db.query("SELECT COUNT(*) FROM m", &[]).unwrap();
        println!(
            "AB bulk-insert columnar: 200000 rows in {:.3}s ({:.0} rows/s) count={:?}",
            secs,
            200_000.0 / secs,
            count[0].values[0]
        );

        // Row-store twin for context.
        let db2 = test_db();
        db2.execute("CREATE TABLE m (id INT8 PRIMARY KEY, v INT8, g INT8, w INT8)")
            .unwrap();
        let secs_row = bulk_insert(&db2, "m", 200_000, 1_000);
        println!(
            "AB bulk-insert row-store twin: 200000 rows in {:.3}s ({:.0} rows/s)",
            secs_row,
            200_000.0 / secs_row
        );
    }

    #[test]
    #[ignore]
    fn ab_columnar_aggregate_500k() {
        let db = test_db();
        db.execute(
            "CREATE TABLE a (id INT8 PRIMARY KEY, v INT8 STORAGE COLUMNAR, g INT8 STORAGE COLUMNAR, \
             w INT8 STORAGE COLUMNAR)",
        )
        .unwrap();
        let seed_secs = bulk_insert(&db, "a", 500_000, 1_000);
        println!("AB aggregate seed: 500000 rows in {:.3}s", seed_secs);

        for (name, sql) in [
            (
                "grouped count/sum (walk->presence)",
                "SELECT g, COUNT(*), SUM(v) FROM a GROUP BY g",
            ),
            (
                "filtered grouped, NULL-accepting (walk->presence)",
                "SELECT g, COUNT(*), SUM(v) FROM a WHERE w IS NULL GROUP BY g",
            ),
            (
                "filtered scalar, NULL-accepting (walk->presence)",
                "SELECT COUNT(*), SUM(v) FROM a WHERE w IS NULL",
            ),
            (
                "unfiltered scalar (batch-direct)",
                "SELECT SUM(v), MIN(v), MAX(v) FROM a",
            ),
            ("zone-prunable range", "SELECT COUNT(*), SUM(v) FROM a WHERE v < 1000"),
        ] {
            let (best, rows) = time_query(&db, sql, 5);
            println!("AB aggregate [{name}]: best {:.1}ms ({rows} rows) :: {sql}", best * 1e3);
        }
    }
}
