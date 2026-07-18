//! Item #4 opportunity measurement (NOT a correctness test — ignored):
//!
//!   cargo test --release --test aggjoin_prune_opportunity_bench -- --ignored --nocapture --test-threads=1
//!
//! Aggregate-over-join currently reads EVERY column of both join sides. This
//! measures the ceiling of "prune to only the columns the aggregate + join keys
//! need" by comparing a WIDE fact table (many unused TEXT columns) against a
//! NARROW twin holding only the referenced columns — same GROUP BY/SUM over a
//! join. narrow_time ~= what column pruning would achieve; wide/narrow = the win.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod aggjoin {
    use heliosdb_nano::{Config, EmbeddedDatabase, Value};
    use std::time::Instant;

    fn test_db() -> EmbeddedDatabase {
        let mut c = Config::default();
        c.storage.memory_only = true;
        c.storage.wal_enabled = false;
        EmbeddedDatabase::with_config(c).expect("db")
    }

    fn pad(i: i64, n: usize) -> String {
        format!("{i:0width$}", width = n)
    }

    fn time_query(db: &EmbeddedDatabase, sql: &str, reps: usize) -> f64 {
        let _ = db.query(sql, &[]).unwrap();
        let mut best = f64::MAX;
        for rep in 0..reps {
            let s = format!("{sql} /* {rep} */");
            let t = Instant::now();
            let _ = db.query(&s, &[]).unwrap();
            best = best.min(t.elapsed().as_secs_f64());
        }
        best
    }

    #[test]
    #[ignore]
    fn aggjoin_prune_opportunity() {
        const FACT: i64 = 500_000;
        const DIM: i64 = 1_000;
        let db = test_db();

        // Dim table (small, join target).
        db.execute("CREATE TABLE dim (k INT8 PRIMARY KEY, label TEXT)").unwrap();
        {
            let sql = "INSERT INTO dim (k, label) VALUES ($1, $2)";
            let batch: Vec<Vec<Value>> = (0..DIM)
                .map(|i| vec![Value::Int8(i), Value::String(pad(i, 20))])
                .collect();
            db.execute_many_params(sql, &batch).unwrap();
        }

        // WIDE fact: k (join), g (group), v (measure) + 8 unused TEXT columns.
        db.execute(
            "CREATE TABLE wide (id INT8 PRIMARY KEY, k INT8, g INT8, v INT8, \
             t1 TEXT, t2 TEXT, t3 TEXT, t4 TEXT, t5 TEXT, t6 TEXT, t7 TEXT, t8 TEXT)",
        )
        .unwrap();
        // NARROW twin: only the referenced columns.
        db.execute("CREATE TABLE narrow (id INT8 PRIMARY KEY, k INT8, g INT8, v INT8)")
            .unwrap();

        let wide_sql = "INSERT INTO wide (id,k,g,v,t1,t2,t3,t4,t5,t6,t7,t8) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)";
        let narrow_sql = "INSERT INTO narrow (id,k,g,v) VALUES ($1,$2,$3,$4)";
        let per = 2_000i64;
        let mut id = 0i64;
        while id < FACT {
            let end = (id + per).min(FACT);
            let mut wide_b = Vec::new();
            let mut narrow_b = Vec::new();
            for i in id..end {
                let (k, g, v) = (i % DIM, i % 20, (i * 7) % 1000);
                narrow_b.push(vec![Value::Int8(i), Value::Int8(k), Value::Int8(g), Value::Int8(v)]);
                let mut row = vec![Value::Int8(i), Value::Int8(k), Value::Int8(g), Value::Int8(v)];
                for c in 0..8 {
                    row.push(Value::String(pad(i + c, 40)));
                }
                wide_b.push(row);
            }
            db.execute_many_params(wide_sql, &wide_b).unwrap();
            db.execute_many_params(narrow_sql, &narrow_b).unwrap();
            id = end;
        }

        println!("\n=== Item #4 aggregate-over-join pruning opportunity ({FACT} fact / {DIM} dim) ===");
        let q = |t: &str| format!("SELECT f.g, COUNT(*), SUM(f.v) FROM {t} f JOIN dim d ON f.k = d.k GROUP BY f.g");
        let wide_ms = time_query(&db, &q("wide"), 4) * 1e3;
        let narrow_ms = time_query(&db, &q("narrow"), 4) * 1e3;
        println!("wide fact (12 cols, 8 unused TEXT): {wide_ms:.1} ms", wide_ms = wide_ms);
        println!(
            "narrow twin (4 cols, all used):     {narrow_ms:.1} ms",
            narrow_ms = narrow_ms
        );
        let ceiling = wide_ms / narrow_ms;
        println!("PRUNE CEILING = {ceiling:.1}x  (wide/narrow — the most column pruning could win)");
        println!(
            "  {} (>= ~1.5x makes item #4 worthwhile; < ~1.2x => scan cost isn't column-count-bound)",
            if ceiling >= 1.5 {
                "WORTHWHILE"
            } else {
                "MARGINAL - reconsider"
            }
        );
    }
}
