//! R3.2 parallel partial aggregation equivalence tests.
//!
//! Forces the rayon parallel aggregation paths on (low threshold) and checks
//! the storage-pushdown aggregates against values computed independently in
//! Rust. Covers the row-store paths (primitive COUNT/SUM/AVG, text grouped
//! count/sum, general grouped/ungrouped) and the columnar paths (unfiltered,
//! grouped count/sum specialization, driver-predicate filtered).
//!
//! Kept as ONE test fn: the threshold env var is latched into a `Lazy` on
//! first use, so it must be set before any query in this process.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod parallel_agg {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    const ROWS: i64 = 6_000;
    const GROUPS: i64 = 7;

    fn to_i64(v: &Value) -> i64 {
        match v {
            Value::Int2(n) => *n as i64,
            Value::Int4(n) => *n as i64,
            Value::Int8(n) => *n,
            other => panic!("Expected integer value, got {:?}", other),
        }
    }

    fn to_f64(v: &Value) -> f64 {
        match v {
            Value::Float4(f) => *f as f64,
            Value::Float8(f) => *f,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            Value::Numeric(s) => s.parse().unwrap(),
            other => panic!("Expected float value, got {:?}", other),
        }
    }

    fn to_str(v: &Value) -> &str {
        match v {
            Value::String(s) => s.as_str(),
            other => panic!("Expected string value, got {:?}", other),
        }
    }

    /// Row i: grp = "g{i%7}", amount = (i*13)%5000 (NULL when i%97==0),
    /// age = 18 + i%60.
    fn amount(i: i64) -> Option<i64> {
        if i % 97 == 0 {
            None
        } else {
            Some((i * 13) % 5000)
        }
    }

    fn age(i: i64) -> i64 {
        18 + (i % 60)
    }

    fn grp(i: i64) -> String {
        format!("g{}", i % GROUPS)
    }

    fn seed(db: &EmbeddedDatabase, table: &str) {
        db.execute("BEGIN").unwrap();
        for i in 0..ROWS {
            let amount_sql = match amount(i) {
                Some(a) => a.to_string(),
                None => "NULL".to_string(),
            };
            db.execute(&format!(
                "INSERT INTO {table} (id, grp, amount, age) VALUES ({i}, '{}', {amount_sql}, {})",
                grp(i),
                age(i)
            ))
            .unwrap();
        }
        db.execute("COMMIT").unwrap();
    }

    fn check_aggregates(db: &EmbeddedDatabase, table: &str) {
        // Expected values, computed independently.
        let exp_count = ROWS;
        let exp_sum: i64 = (0..ROWS).filter_map(amount).sum();
        let exp_avg_age: f64 = (0..ROWS).map(age).sum::<i64>() as f64 / ROWS as f64;
        let mut exp_grp_count = vec![0i64; GROUPS as usize];
        let mut exp_grp_sum = vec![0i64; GROUPS as usize];
        let mut exp_grp_min_age = vec![i64::MAX; GROUPS as usize];
        let mut exp_grp_max_age = vec![i64::MIN; GROUPS as usize];
        for i in 0..ROWS {
            let g = (i % GROUPS) as usize;
            exp_grp_count[g] += 1;
            exp_grp_sum[g] += amount(i).unwrap_or(0);
            exp_grp_min_age[g] = exp_grp_min_age[g].min(age(i));
            exp_grp_max_age[g] = exp_grp_max_age[g].max(age(i));
        }
        let exp_filter_count: i64 = (0..ROWS).filter(|&i| age(i) > 40).count() as i64;
        let exp_filter_sum: i64 = (0..ROWS).filter(|&i| age(i) > 40).filter_map(amount).sum();

        // 1) Ungrouped COUNT/SUM/AVG (row-store: primitive count_sum_avg path;
        //    columnar: unfiltered batch path).
        let rows = db
            .query(&format!("SELECT COUNT(*), SUM(amount), AVG(age) FROM {table}"), &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(&rows[0].values[0]), exp_count, "{table} COUNT(*)");
        assert_eq!(to_i64(&rows[0].values[1]), exp_sum, "{table} SUM(amount)");
        let got_avg = to_f64(&rows[0].values[2]);
        assert!(
            (got_avg - exp_avg_age).abs() < 1e-9,
            "{table} AVG(age): got {got_avg}, expected {exp_avg_age}"
        );

        // 2) Text-grouped COUNT/SUM (row-store: text fast path; columnar:
        //    grouped count/sum specialization). Output must be sorted by group.
        let rows = db
            .query(
                &format!("SELECT grp, COUNT(*), SUM(amount) FROM {table} GROUP BY grp"),
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), GROUPS as usize, "{table} group count");
        for (g, row) in rows.iter().enumerate() {
            assert_eq!(to_str(&row.values[0]), format!("g{g}"), "{table} group ordering");
            assert_eq!(to_i64(&row.values[1]), exp_grp_count[g], "{table} g{g} COUNT");
            assert_eq!(to_i64(&row.values[2]), exp_grp_sum[g], "{table} g{g} SUM");
        }

        // 3) General grouped MIN/MAX (general grouped path with merge).
        let rows = db
            .query(
                &format!("SELECT grp, MIN(age), MAX(age) FROM {table} GROUP BY grp"),
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), GROUPS as usize);
        for (g, row) in rows.iter().enumerate() {
            assert_eq!(to_str(&row.values[0]), format!("g{g}"));
            assert_eq!(to_i64(&row.values[1]), exp_grp_min_age[g], "{table} g{g} MIN(age)");
            assert_eq!(to_i64(&row.values[2]), exp_grp_max_age[g], "{table} g{g} MAX(age)");
        }

        // 4) Filtered ungrouped (row-store general path; columnar driver path).
        let rows = db
            .query(
                &format!("SELECT COUNT(*), SUM(amount) FROM {table} WHERE age > 40"),
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(&rows[0].values[0]), exp_filter_count, "{table} filtered COUNT");
        assert_eq!(to_i64(&rows[0].values[1]), exp_filter_sum, "{table} filtered SUM");

        // 5) Filtered grouped (row-store grouped; columnar driver grouped).
        let rows = db
            .query(
                &format!("SELECT grp, COUNT(*) FROM {table} WHERE age > 40 GROUP BY grp"),
                &[],
            )
            .unwrap();
        let mut exp_filter_grp = vec![0i64; GROUPS as usize];
        for i in (0..ROWS).filter(|&i| age(i) > 40) {
            exp_filter_grp[(i % GROUPS) as usize] += 1;
        }
        assert_eq!(rows.len(), GROUPS as usize);
        for (g, row) in rows.iter().enumerate() {
            assert_eq!(to_str(&row.values[0]), format!("g{g}"));
            assert_eq!(to_i64(&row.values[1]), exp_filter_grp[g], "{table} filtered g{g} COUNT");
        }

        // 6) DISTINCT stays correct (bails to the serial path by design).
        let rows = db
            .query(&format!("SELECT COUNT(DISTINCT grp) FROM {table}"), &[])
            .unwrap();
        assert_eq!(to_i64(&rows[0].values[0]), GROUPS, "{table} COUNT(DISTINCT)");
    }

    #[test]
    fn parallel_partial_aggregation_matches_expected_values() {
        // Engage the parallel paths at a low row count. Must happen before the
        // first query in this process (the threshold is latched lazily).
        std::env::set_var("HELIOS_AGG_PARALLEL_THRESHOLD", "500");

        let db = EmbeddedDatabase::new_in_memory().unwrap();

        // Row-store table.
        db.execute("CREATE TABLE t_row (id INTEGER PRIMARY KEY, grp TEXT, amount INTEGER, age INTEGER)")
            .unwrap();
        seed(&db, "t_row");
        check_aggregates(&db, "t_row");

        // Columnar side-store table.
        db.execute(
            "CREATE TABLE t_col (
                id INTEGER PRIMARY KEY,
                grp TEXT STORAGE COLUMNAR,
                amount INTEGER STORAGE COLUMNAR,
                age INTEGER STORAGE COLUMNAR
            )",
        )
        .unwrap();
        seed(&db, "t_col");
        check_aggregates(&db, "t_col");
    }
}
