//! R3.1 zone-map pruning equivalence tests.
//!
//! A `STORAGE COLUMNAR` table and a default-storage twin are seeded with
//! identical data; every query must return identical (normalized) results on
//! both. The columnar `v` column is clustered by insertion order so selective
//! range/eq predicates actually prune batches; `noise` is deliberately
//! unclustered (no batch can be pruned); `flag` is BOOLEAN (outside the
//! orderable zone-stats set, so min/max pruning is disabled by construction);
//! `w` is NULL-heavy.
//!
//! Run matrix (env vars are latched into `Lazy`s on first use, so each
//! setting needs its own process):
//!   cargo test --test zone_map_pruning_tests -- --test-threads=1
//!   HELIOS_AGG_SERIAL=1   cargo test --test zone_map_pruning_tests -- --test-threads=1
//!   HELIOS_ZONE_MAP_OFF=1 cargo test --test zone_map_pruning_tests -- --test-threads=1
//!
//! The lazy-backfill path (`colz:` keys appearing on first pruned scan over
//! legacy data) is not reachable through the public API — it is unit-tested
//! in-module in `src/storage/columnar.rs`
//! (`test_lazy_backfill_writes_stats_and_marker`).

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod zone_map_pruning {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    /// ~6 columnar batches (BATCH_SIZE = 1024).
    const ROWS: i64 = 6_000;
    const GROUPS: i64 = 7;

    fn w(i: i64) -> Option<i64> {
        if i % 97 == 0 {
            None
        } else {
            Some((i * 13) % 5000)
        }
    }

    fn noise(i: i64) -> i64 {
        (i * 7919) % ROWS
    }

    fn grp(i: i64) -> String {
        format!("g{}", i % GROUPS)
    }

    fn cst(i: i64) -> i64 {
        if i < ROWS / 2 {
            5
        } else {
            9
        }
    }

    fn seed(db: &EmbeddedDatabase, table: &str) {
        db.execute("BEGIN").unwrap();
        for i in 0..ROWS {
            let w_sql = match w(i) {
                Some(x) => x.to_string(),
                None => "NULL".to_string(),
            };
            db.execute(&format!(
                "INSERT INTO {table} (id, v, w, noise, grp, cst, flag) \
                 VALUES ({i}, {i}, {w_sql}, {}, '{}', {}, {})",
                noise(i),
                grp(i),
                cst(i),
                i % 2 == 0
            ))
            .unwrap();
        }
        db.execute("COMMIT").unwrap();
    }

    /// Normalize a value so Int4/Int8 (etc.) representation differences
    /// between the row and columnar execution paths do not matter.
    fn norm(v: &Value) -> String {
        match v {
            Value::Null => "NULL".to_string(),
            Value::Boolean(b) => b.to_string(),
            Value::Int2(n) => (*n as i64).to_string(),
            Value::Int4(n) => (*n as i64).to_string(),
            Value::Int8(n) => n.to_string(),
            Value::Float4(f) => format!("{:.6}", *f as f64),
            Value::Float8(f) => format!("{:.6}", f),
            Value::Numeric(s) => match s.parse::<f64>() {
                Ok(f) => format!("{:.6}", f),
                Err(_) => s.clone(),
            },
            Value::String(s) => format!("'{s}'"),
            other => format!("{other:?}"),
        }
    }

    /// Run the same query against both tables and assert identical results
    /// (sorted row-wise, so ORDER BY is not required).
    fn assert_equivalent(db: &EmbeddedDatabase, label: &str, sql_template: &str) {
        let run = |table: &str| -> Vec<Vec<String>> {
            let sql = sql_template.replace("{T}", table);
            let mut rows: Vec<Vec<String>> = db
                .query(&sql, &[])
                .unwrap_or_else(|e| panic!("{label}: query failed on {table}: {e}"))
                .iter()
                .map(|row| row.values.iter().map(norm).collect())
                .collect();
            rows.sort();
            rows
        };
        let col = run("t_zone");
        let row = run("t_plain");
        assert_eq!(col, row, "{label}: columnar vs row results differ");
    }

    #[test]
    fn zone_map_pruning_matches_row_store_twin() {
        // Engage the R3.2 parallel aggregation paths at a low row count so
        // pruning is exercised under both the serial and parallel regimes in
        // the same suite (HELIOS_AGG_SERIAL=1 forces serial in a separate
        // run). Must be set before the first query in this process.
        std::env::set_var("HELIOS_AGG_PARALLEL_THRESHOLD", "500");

        let db = EmbeddedDatabase::new_in_memory().unwrap();

        db.execute(
            "CREATE TABLE t_zone (
                id INTEGER PRIMARY KEY,
                v INTEGER STORAGE COLUMNAR,
                w INTEGER STORAGE COLUMNAR,
                noise INTEGER STORAGE COLUMNAR,
                grp TEXT STORAGE COLUMNAR,
                cst INTEGER STORAGE COLUMNAR,
                flag BOOLEAN STORAGE COLUMNAR
            )",
        )
        .unwrap();
        db.execute(
            "CREATE TABLE t_plain (
                id INTEGER PRIMARY KEY,
                v INTEGER,
                w INTEGER,
                noise INTEGER,
                grp TEXT,
                cst INTEGER,
                flag BOOLEAN
            )",
        )
        .unwrap();
        seed(&db, "t_zone");
        seed(&db, "t_plain");

        // --- selective range predicates on the clustered column (prunes) ---
        assert_equivalent(
            &db,
            "range_agg_matching",
            "SELECT COUNT(*), SUM(w), MIN(v), MAX(v) FROM {T} WHERE v >= 5500",
        );
        assert_equivalent(
            &db,
            "range_select_matching",
            "SELECT v, w FROM {T} WHERE v BETWEEN 1000 AND 1100",
        );
        assert_equivalent(
            &db,
            "range_agg_no_match",
            "SELECT COUNT(*), SUM(w) FROM {T} WHERE v > 999999",
        );
        assert_equivalent(&db, "range_select_no_match", "SELECT v, w FROM {T} WHERE v < -5");
        assert_equivalent(
            &db,
            "range_lower_edge",
            "SELECT COUNT(*), SUM(v) FROM {T} WHERE v <= 1023",
        );

        // --- equality in/out of domain ---
        assert_equivalent(&db, "eq_inside", "SELECT v, w, grp FROM {T} WHERE v = 4242");
        assert_equivalent(&db, "eq_outside", "SELECT v, w FROM {T} WHERE v = 123456");
        assert_equivalent(&db, "eq_agg", "SELECT COUNT(*), SUM(w) FROM {T} WHERE v = 4242");

        // --- IN list partially out of range ---
        assert_equivalent(
            &db,
            "in_list",
            "SELECT v FROM {T} WHERE v IN (10, 2048, 99999, 5999, -1)",
        );

        // --- NotEq against per-range-constant column (all-equal batches prune) ---
        assert_equivalent(&db, "noteq_constant", "SELECT COUNT(*) FROM {T} WHERE cst != 5");
        assert_equivalent(
            &db,
            "noteq_constant_filtered_agg",
            "SELECT COUNT(*), SUM(w) FROM {T} WHERE cst != 9 AND v < 100",
        );

        // --- NULL-heavy column ---
        assert_equivalent(&db, "is_null", "SELECT COUNT(*) FROM {T} WHERE w IS NULL");
        assert_equivalent(&db, "is_not_null", "SELECT COUNT(*) FROM {T} WHERE w IS NOT NULL");
        assert_equivalent(
            &db,
            "nullable_range",
            "SELECT COUNT(*), SUM(v) FROM {T} WHERE w >= 4900",
        );

        // --- unsupported min/max type (BOOLEAN): correctness without pruning ---
        assert_equivalent(&db, "bool_eq", "SELECT COUNT(*) FROM {T} WHERE flag = TRUE");
        assert_equivalent(
            &db,
            "bool_and_range",
            "SELECT COUNT(*), SUM(w) FROM {T} WHERE flag = FALSE AND v >= 5800",
        );

        // --- unclustered column: every batch overlaps, nothing prunes ---
        assert_equivalent(
            &db,
            "noise_range",
            "SELECT COUNT(*), SUM(w) FROM {T} WHERE noise >= 5900",
        );

        // --- multi-predicate across columns (one prunes, one does not) ---
        assert_equivalent(
            &db,
            "multi_predicate",
            "SELECT COUNT(*), SUM(w) FROM {T} WHERE v >= 5500 AND noise < 3000",
        );

        // --- text predicates (driver-batch path) ---
        assert_equivalent(&db, "text_eq", "SELECT COUNT(*), SUM(w) FROM {T} WHERE grp = 'g3'");
        assert_equivalent(
            &db,
            "text_eq_grouped",
            "SELECT grp, COUNT(*), SUM(w) FROM {T} WHERE v >= 5000 GROUP BY grp",
        );

        // --- grouped count/sum specialization with a pruning filter ---
        assert_equivalent(
            &db,
            "grouped_count_sum",
            "SELECT cst, COUNT(*), SUM(w) FROM {T} WHERE v >= 5500 GROUP BY cst",
        );

        // --- updates must refresh stats (stale stats would mis-prune) ---
        for table in ["t_zone", "t_plain"] {
            db.execute(&format!("UPDATE {table} SET v = 100000 + id WHERE id < 10"))
                .unwrap();
        }
        assert_equivalent(
            &db,
            "post_update_new_range",
            "SELECT COUNT(*), SUM(v) FROM {T} WHERE v >= 100000",
        );
        assert_equivalent(
            &db,
            "post_update_old_range",
            "SELECT COUNT(*), SUM(v) FROM {T} WHERE v < 10",
        );

        // --- deletes write NULL slots; pruned aggregates must stay exact ---
        for table in ["t_zone", "t_plain"] {
            db.execute(&format!("DELETE FROM {table} WHERE v >= 5900")).unwrap();
        }
        assert_equivalent(
            &db,
            "post_delete_range",
            "SELECT COUNT(*), SUM(w) FROM {T} WHERE v >= 5500",
        );
        assert_equivalent(&db, "post_delete_count", "SELECT COUNT(*) FROM {T}");
        assert_equivalent(
            &db,
            "post_delete_no_match",
            "SELECT COUNT(*), SUM(w) FROM {T} WHERE v >= 5900",
        );
    }
}
