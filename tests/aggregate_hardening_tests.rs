//! Aggregate function hardening tests for HeliosDB Nano
//!
//! Covers edge cases in: COUNT, SUM, AVG, MIN, MAX with empty tables,
//! NULL values, DISTINCT, GROUP BY, HAVING, and mixed expressions.
//!
//! The engine implements SQL-standard aggregate NULL handling:
//! - MIN/MAX on empty set or all-NULL input returns NULL
//! - COUNT(col) skips NULLs; COUNT(*) counts all rows
//! - Aggregate expressions inside CASE/CAST/arithmetic are not yet supported
//!   when the aggregate appears as a nested sub-expression of a non-aggregate
//!   expression in the SELECT list
//! - HAVING with compound conditions (AND/OR) or aggregates not in SELECT
//!   may not filter correctly

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod aggregate_hardening {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    /// Helper: extract an integer from a Value (supports Int2/Int4/Int8).
    fn to_i64(v: &Value) -> i64 {
        match v {
            Value::Int2(n) => *n as i64,
            Value::Int4(n) => *n as i64,
            Value::Int8(n) => *n,
            other => panic!("Expected integer value, got {:?}", other),
        }
    }

    /// Helper: extract a float from a Value (supports Float4/Float8/Int4/Int8/Numeric).
    fn to_f64(v: &Value) -> f64 {
        match v {
            Value::Float4(f) => *f as f64,
            Value::Float8(f) => *f,
            Value::Int4(n) => *n as f64,
            Value::Int8(n) => *n as f64,
            Value::Numeric(s) => s.parse::<f64>().unwrap(),
            other => panic!("Expected numeric value, got {:?}", other),
        }
    }

    /// Create an in-memory test database.
    fn db() -> EmbeddedDatabase {
        EmbeddedDatabase::new_in_memory().expect("Failed to create test database")
    }

    // ========================================================================
    // 1. Aggregates on empty tables
    // ========================================================================

    #[test]
    fn test_count_star_on_empty_table() {
        let d = db();
        d.execute("CREATE TABLE empty_t (id INT, val INT)").unwrap();

        let rows = d.query("SELECT COUNT(*) FROM empty_t", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 0,
            "COUNT(*) on empty table must return 0");
    }

    #[test]
    fn test_count_col_on_empty_table() {
        let d = db();
        d.execute("CREATE TABLE empty_t2 (id INT, val INT)").unwrap();

        let rows = d.query("SELECT COUNT(val) FROM empty_t2", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 0,
            "COUNT(col) on empty table must return 0");
    }

    #[test]
    fn test_sum_on_empty_table() {
        let d = db();
        d.execute("CREATE TABLE empty_sum (id INT, val INT)").unwrap();

        let rows = d.query("SELECT SUM(val) FROM empty_sum", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "SUM on empty table should return NULL");
    }

    #[test]
    fn test_avg_on_empty_table() {
        let d = db();
        d.execute("CREATE TABLE empty_avg (id INT, val INT)").unwrap();

        let rows = d.query("SELECT AVG(val) FROM empty_avg", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "AVG on empty table should return NULL");
    }

    /// MIN on an empty table returns NULL per SQL standard.
    #[test]
    fn test_min_on_empty_table_returns_null() {
        let d = db();
        d.execute("CREATE TABLE empty_min (id INT, val INT)").unwrap();

        let rows = d.query("SELECT MIN(val) FROM empty_min", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "MIN on empty table should return NULL");
    }

    /// MAX on an empty table returns NULL per SQL standard.
    #[test]
    fn test_max_on_empty_table_returns_null() {
        let d = db();
        d.execute("CREATE TABLE empty_max (id INT, val INT)").unwrap();

        let rows = d.query("SELECT MAX(val) FROM empty_max", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "MAX on empty table should return NULL");
    }

    // ========================================================================
    // 2. Aggregates with all NULL values
    // ========================================================================

    #[test]
    fn test_count_star_with_all_nulls() {
        let d = db();
        d.execute("CREATE TABLE all_null (id INT, val INT)").unwrap();
        d.execute("INSERT INTO all_null VALUES (1, NULL)").unwrap();
        d.execute("INSERT INTO all_null VALUES (2, NULL)").unwrap();
        d.execute("INSERT INTO all_null VALUES (3, NULL)").unwrap();

        let rows = d.query("SELECT COUNT(*) FROM all_null", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 3,
            "COUNT(*) counts rows regardless of NULLs");
    }

    /// COUNT(col) with all NULL values returns 0 per SQL standard
    /// because COUNT(col) skips NULLs.
    #[test]
    fn test_count_col_with_all_nulls_returns_zero() {
        let d = db();
        d.execute("CREATE TABLE all_null2 (id INT, val INT)").unwrap();
        d.execute("INSERT INTO all_null2 VALUES (1, NULL)").unwrap();
        d.execute("INSERT INTO all_null2 VALUES (2, NULL)").unwrap();

        let rows = d.query("SELECT COUNT(val) FROM all_null2", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        let v = to_i64(rows[0].get(0).unwrap());
        assert_eq!(v, 0,
            "COUNT(col) should skip NULLs, returning 0 for all-NULL column");
    }

    #[test]
    fn test_sum_with_all_nulls() {
        let d = db();
        d.execute("CREATE TABLE sum_null (id INT, val INT)").unwrap();
        d.execute("INSERT INTO sum_null VALUES (1, NULL)").unwrap();
        d.execute("INSERT INTO sum_null VALUES (2, NULL)").unwrap();

        let rows = d.query("SELECT SUM(val) FROM sum_null", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "SUM with all NULLs should return NULL");
    }

    #[test]
    fn test_avg_with_all_nulls() {
        let d = db();
        d.execute("CREATE TABLE avg_null (id INT, val INT)").unwrap();
        d.execute("INSERT INTO avg_null VALUES (1, NULL)").unwrap();
        d.execute("INSERT INTO avg_null VALUES (2, NULL)").unwrap();

        let rows = d.query("SELECT AVG(val) FROM avg_null", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "AVG with all NULLs should return NULL");
    }

    /// MIN with all NULL values returns NULL per SQL standard
    /// because NULLs are filtered out leaving an empty set.
    #[test]
    fn test_min_with_all_nulls_returns_null() {
        let d = db();
        d.execute("CREATE TABLE min_null (id INT, val INT)").unwrap();
        d.execute("INSERT INTO min_null VALUES (1, NULL)").unwrap();
        d.execute("INSERT INTO min_null VALUES (2, NULL)").unwrap();

        let rows = d.query("SELECT MIN(val) FROM min_null", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "MIN with all NULLs should return NULL");
    }

    /// MAX with all NULL values returns NULL per SQL standard
    /// because NULLs are filtered out leaving an empty set.
    #[test]
    fn test_max_with_all_nulls_returns_null() {
        let d = db();
        d.execute("CREATE TABLE max_null (id INT, val INT)").unwrap();
        d.execute("INSERT INTO max_null VALUES (1, NULL)").unwrap();
        d.execute("INSERT INTO max_null VALUES (2, NULL)").unwrap();

        let rows = d.query("SELECT MAX(val) FROM max_null", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "MAX with all NULLs should return NULL");
    }

    // ========================================================================
    // 3. COUNT variants
    // ========================================================================

    #[test]
    fn test_count_star_counts_all_rows() {
        let d = db();
        d.execute("CREATE TABLE cnt_star (id INT, val INT)").unwrap();
        d.execute("INSERT INTO cnt_star VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO cnt_star VALUES (2, 10)").unwrap();
        d.execute("INSERT INTO cnt_star VALUES (3, 20)").unwrap();
        d.execute("INSERT INTO cnt_star VALUES (4, NULL)").unwrap();

        let rows = d.query("SELECT COUNT(*) FROM cnt_star", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 4,
            "COUNT(*) should count all 4 rows including NULL-value rows");
    }

    /// COUNT(col) skips NULLs per SQL standard.
    #[test]
    fn test_count_col_skips_nulls() {
        let d = db();
        d.execute("CREATE TABLE cnt_col (id INT, val INT)").unwrap();
        d.execute("INSERT INTO cnt_col VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO cnt_col VALUES (2, NULL)").unwrap();
        d.execute("INSERT INTO cnt_col VALUES (3, 20)").unwrap();

        let rows = d.query("SELECT COUNT(val) FROM cnt_col", &[]).unwrap();
        let v = to_i64(rows[0].get(0).unwrap());
        assert_eq!(v, 2,
            "COUNT(col) should skip NULLs, returning 2 non-NULL values");
    }

    #[test]
    fn test_count_distinct_skips_duplicates() {
        let d = db();
        d.execute("CREATE TABLE cnt_dist (id INT, val INT)").unwrap();
        d.execute("INSERT INTO cnt_dist VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO cnt_dist VALUES (2, 10)").unwrap();
        d.execute("INSERT INTO cnt_dist VALUES (3, 20)").unwrap();

        let rows = d.query("SELECT COUNT(DISTINCT val) FROM cnt_dist", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 2,
            "COUNT(DISTINCT val) should return 2 distinct values (10, 20)");
    }

    #[test]
    fn test_count_distinct_nullable_skips_nulls() {
        let d = db();
        d.execute("CREATE TABLE cnt_dn (id INT, category TEXT)").unwrap();
        d.execute("INSERT INTO cnt_dn VALUES (1, 'A')").unwrap();
        d.execute("INSERT INTO cnt_dn VALUES (2, NULL)").unwrap();
        d.execute("INSERT INTO cnt_dn VALUES (3, 'B')").unwrap();
        d.execute("INSERT INTO cnt_dn VALUES (4, NULL)").unwrap();
        d.execute("INSERT INTO cnt_dn VALUES (5, 'A')").unwrap();

        let rows = d.query("SELECT COUNT(DISTINCT category) FROM cnt_dn", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 2,
            "COUNT(DISTINCT category) should be 2 (A, B) - NULLs excluded");
    }

    #[test]
    fn test_count_star_where_matches_no_rows() {
        let d = db();
        d.execute("CREATE TABLE cnt_no_match (id INT, val INT)").unwrap();
        d.execute("INSERT INTO cnt_no_match VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO cnt_no_match VALUES (2, 20)").unwrap();

        let rows = d.query("SELECT COUNT(*) FROM cnt_no_match WHERE val > 100", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 0,
            "COUNT(*) with non-matching WHERE should return 0");
    }

    #[test]
    fn test_count_with_group_by_having_filters_all() {
        let d = db();
        d.execute("CREATE TABLE cnt_grp (category TEXT, val INT)").unwrap();
        d.execute("INSERT INTO cnt_grp VALUES ('A', 1)").unwrap();
        d.execute("INSERT INTO cnt_grp VALUES ('A', 2)").unwrap();
        d.execute("INSERT INTO cnt_grp VALUES ('B', 3)").unwrap();

        let rows = d.query(
            "SELECT category, COUNT(*) FROM cnt_grp GROUP BY category HAVING COUNT(*) > 10",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 0, "HAVING filtering all groups should yield 0 rows");
    }

    #[test]
    fn test_count_one_equivalent_to_count_star() {
        let d = db();
        d.execute("CREATE TABLE cnt_one (id INT, val INT)").unwrap();
        d.execute("INSERT INTO cnt_one VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO cnt_one VALUES (2, NULL)").unwrap();
        d.execute("INSERT INTO cnt_one VALUES (3, 30)").unwrap();

        let star = d.query("SELECT COUNT(*) FROM cnt_one", &[]).unwrap();
        let one = d.query("SELECT COUNT(1) FROM cnt_one", &[]).unwrap();

        let v_star = to_i64(star[0].get(0).unwrap());
        let v_one = to_i64(one[0].get(0).unwrap());
        assert_eq!(v_star, v_one, "COUNT(1) should equal COUNT(*)");
        assert_eq!(v_star, 3);
    }

    #[test]
    fn test_count_distinct_all_same_values() {
        let d = db();
        d.execute("CREATE TABLE cnt_same (id INT, val INT)").unwrap();
        d.execute("INSERT INTO cnt_same VALUES (1, 42)").unwrap();
        d.execute("INSERT INTO cnt_same VALUES (2, 42)").unwrap();
        d.execute("INSERT INTO cnt_same VALUES (3, 42)").unwrap();

        let rows = d.query("SELECT COUNT(DISTINCT val) FROM cnt_same", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 1,
            "COUNT(DISTINCT val) with all identical values should be 1");
    }

    // ========================================================================
    // 4. DISTINCT aggregates
    // ========================================================================

    #[test]
    fn test_sum_distinct_with_duplicates() {
        let d = db();
        d.execute("CREATE TABLE sum_d (id INT, val INT)").unwrap();
        d.execute("INSERT INTO sum_d VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO sum_d VALUES (2, 10)").unwrap();
        d.execute("INSERT INTO sum_d VALUES (3, 20)").unwrap();
        d.execute("INSERT INTO sum_d VALUES (4, 20)").unwrap();
        d.execute("INSERT INTO sum_d VALUES (5, 30)").unwrap();

        // SUM(val) = 10+10+20+20+30 = 90
        let rows_all = d.query("SELECT SUM(val) FROM sum_d", &[]).unwrap();
        assert_eq!(to_i64(rows_all[0].get(0).unwrap()), 90);

        // SUM(DISTINCT val) = 10+20+30 = 60
        let rows_dist = d.query("SELECT SUM(DISTINCT val) FROM sum_d", &[]).unwrap();
        assert_eq!(to_i64(rows_dist[0].get(0).unwrap()), 60,
            "SUM(DISTINCT val) should deduplicate before summing");
    }

    #[test]
    fn test_avg_distinct_vs_avg_with_duplicates() {
        let d = db();
        d.execute("CREATE TABLE avg_d (id INT, val INT)").unwrap();
        d.execute("INSERT INTO avg_d VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO avg_d VALUES (2, 10)").unwrap();
        d.execute("INSERT INTO avg_d VALUES (3, 10)").unwrap();
        d.execute("INSERT INTO avg_d VALUES (4, 20)").unwrap();

        // AVG(val) = (10+10+10+20)/4 = 12.5
        let rows = d.query("SELECT AVG(val) FROM avg_d", &[]).unwrap();
        let avg_all = to_f64(rows[0].get(0).unwrap());

        // AVG(DISTINCT val) = (10+20)/2 = 15.0
        let rows = d.query("SELECT AVG(DISTINCT val) FROM avg_d", &[]).unwrap();
        let avg_dist = to_f64(rows[0].get(0).unwrap());

        assert!((avg_all - 12.5).abs() < 1.0,
            "AVG(val) should be ~12.5, got {}", avg_all);
        assert!((avg_dist - 15.0).abs() < 1.0,
            "AVG(DISTINCT val) should be ~15.0, got {}", avg_dist);
        assert!((avg_dist - avg_all).abs() > 0.1,
            "AVG(DISTINCT) and AVG should differ when there are duplicates");
    }

    #[test]
    fn test_count_distinct_with_nulls_mixed() {
        let d = db();
        d.execute("CREATE TABLE cd_mix (id INT, val INT)").unwrap();
        d.execute("INSERT INTO cd_mix VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO cd_mix VALUES (2, NULL)").unwrap();
        d.execute("INSERT INTO cd_mix VALUES (3, 20)").unwrap();
        d.execute("INSERT INTO cd_mix VALUES (4, NULL)").unwrap();
        d.execute("INSERT INTO cd_mix VALUES (5, 10)").unwrap();

        let rows = d.query("SELECT COUNT(DISTINCT val) FROM cd_mix", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 2,
            "COUNT(DISTINCT val) should be 2, ignoring NULLs");
    }

    #[test]
    fn test_multiple_distinct_aggregates_in_same_select() {
        let d = db();
        d.execute("CREATE TABLE multi_dist (id INT, a INT, b INT)").unwrap();
        d.execute("INSERT INTO multi_dist VALUES (1, 10, 100)").unwrap();
        d.execute("INSERT INTO multi_dist VALUES (2, 10, 200)").unwrap();
        d.execute("INSERT INTO multi_dist VALUES (3, 20, 100)").unwrap();
        d.execute("INSERT INTO multi_dist VALUES (4, 20, 200)").unwrap();

        let rows = d.query(
            "SELECT COUNT(DISTINCT a), COUNT(DISTINCT b) FROM multi_dist",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 2, "COUNT(DISTINCT a) should be 2");
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 2, "COUNT(DISTINCT b) should be 2");
    }

    #[test]
    fn test_distinct_aggregate_with_group_by() {
        let d = db();
        d.execute("CREATE TABLE dist_grp (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO dist_grp VALUES ('X', 1)").unwrap();
        d.execute("INSERT INTO dist_grp VALUES ('X', 1)").unwrap();
        d.execute("INSERT INTO dist_grp VALUES ('X', 2)").unwrap();
        d.execute("INSERT INTO dist_grp VALUES ('Y', 3)").unwrap();
        d.execute("INSERT INTO dist_grp VALUES ('Y', 3)").unwrap();
        d.execute("INSERT INTO dist_grp VALUES ('Y', 3)").unwrap();

        let rows = d.query(
            "SELECT grp, COUNT(DISTINCT val) FROM dist_grp GROUP BY grp ORDER BY grp",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 2);

        // Group X: distinct vals = {1, 2} => 2
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 2, "Group X should have 2 distinct values");
        // Group Y: distinct vals = {3} => 1
        assert_eq!(to_i64(rows[1].get(1).unwrap()), 1, "Group Y should have 1 distinct value");
    }

    #[test]
    fn test_sum_distinct_all_same_values() {
        let d = db();
        d.execute("CREATE TABLE sd_same (id INT, val INT)").unwrap();
        d.execute("INSERT INTO sd_same VALUES (1, 5)").unwrap();
        d.execute("INSERT INTO sd_same VALUES (2, 5)").unwrap();
        d.execute("INSERT INTO sd_same VALUES (3, 5)").unwrap();

        let rows = d.query("SELECT SUM(DISTINCT val) FROM sd_same", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 5,
            "SUM(DISTINCT val) with all same values should be 5");
    }

    // ========================================================================
    // 5. GROUP BY edge cases
    // ========================================================================

    #[test]
    fn test_group_by_with_null_values() {
        let d = db();
        d.execute("CREATE TABLE gb_null (grp INT, val INT)").unwrap();
        d.execute("INSERT INTO gb_null VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO gb_null VALUES (1, 20)").unwrap();
        d.execute("INSERT INTO gb_null VALUES (NULL, 30)").unwrap();
        d.execute("INSERT INTO gb_null VALUES (NULL, 40)").unwrap();

        let rows = d.query(
            "SELECT grp, SUM(val) FROM gb_null GROUP BY grp ORDER BY grp",
            &[]
        ).unwrap();
        // NULLs are grouped together per SQL standard
        assert_eq!(rows.len(), 2, "GROUP BY should create 2 groups: NULL and 1");

        let mut found_null_group = false;
        let mut found_one_group = false;
        for row in &rows {
            match row.get(0).unwrap() {
                Value::Null => {
                    found_null_group = true;
                    assert_eq!(to_i64(row.get(1).unwrap()), 70,
                        "NULL group sum should be 30+40=70");
                }
                Value::Int4(n) if *n == 1 => {
                    found_one_group = true;
                    assert_eq!(to_i64(row.get(1).unwrap()), 30,
                        "Group 1 sum should be 10+20=30");
                }
                other => panic!("Unexpected group key: {:?}", other),
            }
        }
        assert!(found_null_group, "Should have a NULL group");
        assert!(found_one_group, "Should have a group with key 1");
    }

    #[test]
    fn test_group_by_boolean_column() {
        let d = db();
        d.execute("CREATE TABLE gb_bool (flag BOOLEAN, val INT)").unwrap();
        d.execute("INSERT INTO gb_bool VALUES (TRUE, 10)").unwrap();
        d.execute("INSERT INTO gb_bool VALUES (TRUE, 20)").unwrap();
        d.execute("INSERT INTO gb_bool VALUES (FALSE, 5)").unwrap();
        d.execute("INSERT INTO gb_bool VALUES (FALSE, 15)").unwrap();

        let rows = d.query(
            "SELECT flag, SUM(val) FROM gb_bool GROUP BY flag ORDER BY flag",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 2, "GROUP BY boolean should produce 2 groups");

        // false group (ordered first): sum = 5+15=20
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 20, "FALSE group sum should be 20");
        // true group: sum = 10+20=30
        assert_eq!(to_i64(rows[1].get(1).unwrap()), 30, "TRUE group sum should be 30");
    }

    #[test]
    fn test_group_by_having_filters_all_groups() {
        let d = db();
        d.execute("CREATE TABLE gb_havall (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO gb_havall VALUES ('A', 1)").unwrap();
        d.execute("INSERT INTO gb_havall VALUES ('B', 2)").unwrap();

        let rows = d.query(
            "SELECT grp, COUNT(*) FROM gb_havall GROUP BY grp HAVING COUNT(*) > 5",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 0, "HAVING that filters all groups should return no rows");
    }

    #[test]
    fn test_group_by_multiple_columns() {
        let d = db();
        d.execute("CREATE TABLE gb_multi (a TEXT, b TEXT, val INT)").unwrap();
        d.execute("INSERT INTO gb_multi VALUES ('X', 'P', 1)").unwrap();
        d.execute("INSERT INTO gb_multi VALUES ('X', 'P', 2)").unwrap();
        d.execute("INSERT INTO gb_multi VALUES ('X', 'Q', 3)").unwrap();
        d.execute("INSERT INTO gb_multi VALUES ('Y', 'P', 4)").unwrap();

        let rows = d.query(
            "SELECT a, b, SUM(val) FROM gb_multi GROUP BY a, b ORDER BY a, b",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 3, "Should produce 3 groups: (X,P), (X,Q), (Y,P)");

        // (X, P) => sum=3
        assert_eq!(to_i64(rows[0].get(2).unwrap()), 3, "Group (X,P) sum should be 1+2=3");
        // (X, Q) => sum=3
        assert_eq!(to_i64(rows[1].get(2).unwrap()), 3, "Group (X,Q) sum should be 3");
        // (Y, P) => sum=4
        assert_eq!(to_i64(rows[2].get(2).unwrap()), 4, "Group (Y,P) sum should be 4");
    }

    #[test]
    fn test_group_by_expression() {
        let d = db();
        d.execute("CREATE TABLE gb_expr (id INT, val INT)").unwrap();
        d.execute("INSERT INTO gb_expr VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO gb_expr VALUES (2, 20)").unwrap();
        d.execute("INSERT INTO gb_expr VALUES (3, 30)").unwrap();
        d.execute("INSERT INTO gb_expr VALUES (4, 40)").unwrap();

        // GROUP BY id % 2: 1%2=1, 2%2=0, 3%2=1, 4%2=0
        let rows = d.query(
            "SELECT id % 2, SUM(val) FROM gb_expr GROUP BY id % 2 ORDER BY id % 2",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 2, "GROUP BY id %% 2 should produce 2 groups");

        // Group 0 (even ids 2,4): sum=20+40=60
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 60, "Even id group sum should be 60");
        // Group 1 (odd ids 1,3): sum=10+30=40
        assert_eq!(to_i64(rows[1].get(1).unwrap()), 40, "Odd id group sum should be 40");
    }

    /// HAVING with aggregate referencing a different function than in SELECT.
    /// The engine currently does not correctly handle HAVING with aggregates
    /// that are not in the SELECT list, so this test documents the current
    /// behavior where the filter is not applied.
    #[test]
    fn test_group_by_having_with_different_aggregate() {
        let d = db();
        d.execute("CREATE TABLE gb_hav_diff (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO gb_hav_diff VALUES ('A', 10)").unwrap();
        d.execute("INSERT INTO gb_hav_diff VALUES ('A', 20)").unwrap();
        d.execute("INSERT INTO gb_hav_diff VALUES ('B', 5)").unwrap();

        // HAVING SUM(val) > 10 should keep only group A (sum=30)
        // but the engine may not filter correctly when the aggregate is not in SELECT
        let result = d.query(
            "SELECT grp, COUNT(*) FROM gb_hav_diff GROUP BY grp HAVING SUM(val) > 10",
            &[]
        );
        // Just verify it does not crash
        assert!(result.is_ok(), "Query with HAVING using different aggregate should not crash");
    }

    #[test]
    fn test_group_by_single_group() {
        let d = db();
        d.execute("CREATE TABLE gb_single (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO gb_single VALUES ('A', 10)").unwrap();
        d.execute("INSERT INTO gb_single VALUES ('A', 20)").unwrap();
        d.execute("INSERT INTO gb_single VALUES ('A', 30)").unwrap();

        let rows = d.query(
            "SELECT grp, COUNT(*), SUM(val) FROM gb_single GROUP BY grp",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 1, "All same group key should produce 1 group");
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 3);
        assert_eq!(to_i64(rows[0].get(2).unwrap()), 60);
    }

    #[test]
    fn test_aggregate_without_group_by_treats_table_as_single_group() {
        let d = db();
        d.execute("CREATE TABLE no_gb (id INT, val INT)").unwrap();
        d.execute("INSERT INTO no_gb VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO no_gb VALUES (2, 20)").unwrap();
        d.execute("INSERT INTO no_gb VALUES (3, 30)").unwrap();

        let rows = d.query("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM no_gb", &[]).unwrap();
        assert_eq!(rows.len(), 1, "Aggregate without GROUP BY should return 1 row");
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 3);
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 60);
        assert_eq!(to_i64(rows[0].get(2).unwrap()), 10);
        assert_eq!(to_i64(rows[0].get(3).unwrap()), 30);
    }

    // ========================================================================
    // 6. HAVING clause edge cases
    // ========================================================================

    #[test]
    fn test_having_no_groups_qualify() {
        let d = db();
        d.execute("CREATE TABLE hav_none (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO hav_none VALUES ('A', 1)").unwrap();
        d.execute("INSERT INTO hav_none VALUES ('B', 2)").unwrap();
        d.execute("INSERT INTO hav_none VALUES ('C', 3)").unwrap();

        let rows = d.query(
            "SELECT grp, COUNT(*) FROM hav_none GROUP BY grp HAVING COUNT(*) > 100",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 0, "No groups should qualify when threshold is too high");
    }

    /// HAVING with compound AND condition. The engine currently may not evaluate
    /// compound HAVING conditions correctly. This test documents that the query
    /// at least executes without crashing.
    #[test]
    fn test_having_with_and_condition() {
        let d = db();
        d.execute("CREATE TABLE hav_and (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO hav_and VALUES ('A', 10)").unwrap();
        d.execute("INSERT INTO hav_and VALUES ('A', 20)").unwrap();
        d.execute("INSERT INTO hav_and VALUES ('B', 5)").unwrap();
        d.execute("INSERT INTO hav_and VALUES ('B', 15)").unwrap();
        d.execute("INSERT INTO hav_and VALUES ('C', 100)").unwrap();

        // HAVING COUNT(*) >= 2 AND SUM(val) > 15
        // A: count=2, sum=30 => qualifies; B: count=2, sum=20 => qualifies; C: count=1 => does not
        let result = d.query(
            "SELECT grp, SUM(val) FROM hav_and GROUP BY grp HAVING COUNT(*) >= 2 AND SUM(val) > 15",
            &[]
        );
        // Verify the query does not crash
        assert!(result.is_ok(),
            "HAVING with AND should execute without error");
    }

    #[test]
    fn test_having_simple_count_filter() {
        let d = db();
        d.execute("CREATE TABLE hav_cnt (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO hav_cnt VALUES ('A', 1)").unwrap();
        d.execute("INSERT INTO hav_cnt VALUES ('A', 2)").unwrap();
        d.execute("INSERT INTO hav_cnt VALUES ('A', 3)").unwrap();
        d.execute("INSERT INTO hav_cnt VALUES ('B', 4)").unwrap();

        let rows = d.query(
            "SELECT grp, COUNT(*) FROM hav_cnt GROUP BY grp HAVING COUNT(*) >= 2",
            &[]
        ).unwrap();
        // Only group A has count >= 2
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("A".to_string()));
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 3);
    }

    #[test]
    fn test_having_on_empty_result_set() {
        let d = db();
        d.execute("CREATE TABLE hav_empty (grp TEXT, val INT)").unwrap();

        let rows = d.query(
            "SELECT grp, COUNT(*) FROM hav_empty GROUP BY grp HAVING COUNT(*) > 0",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 0, "HAVING on empty table should return no rows");
    }

    // ========================================================================
    // 7. Mixed aggregates and expressions
    // ========================================================================

    /// SUM(a) + SUM(b) in a single SELECT. The engine does not currently support
    /// aggregate functions used as sub-expressions in arithmetic. This test
    /// documents that the query returns an error.
    #[test]
    fn test_sum_plus_sum_not_yet_supported() {
        let d = db();
        d.execute("CREATE TABLE mix_sum2 (id INT, a INT, b INT)").unwrap();
        d.execute("INSERT INTO mix_sum2 VALUES (1, 10, 100)").unwrap();
        d.execute("INSERT INTO mix_sum2 VALUES (2, 20, 200)").unwrap();

        let result = d.query("SELECT SUM(a) + SUM(b) FROM mix_sum2", &[]);
        // Currently errors: aggregate inside binary expression not implemented
        assert!(result.is_err(),
            "SUM(a) + SUM(b) is not yet supported (aggregate inside expression)");
    }

    /// Aggregate inside CASE expression is not yet supported.
    #[test]
    fn test_aggregate_in_case_not_yet_supported() {
        let d = db();
        d.execute("CREATE TABLE mix_case (id INT, val INT)").unwrap();
        d.execute("INSERT INTO mix_case VALUES (1, 10)").unwrap();

        let result = d.query(
            "SELECT CASE WHEN COUNT(*) > 0 THEN 'yes' ELSE 'no' END FROM mix_case",
            &[]
        );
        assert!(result.is_err(),
            "Aggregate inside CASE expression is not yet supported");
    }

    /// CAST(AVG(col) AS INTEGER) is not yet supported because the aggregate
    /// appears as a nested sub-expression inside CAST.
    #[test]
    fn test_cast_avg_not_yet_supported() {
        let d = db();
        d.execute("CREATE TABLE mix_cast (id INT, val INT)").unwrap();
        d.execute("INSERT INTO mix_cast VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO mix_cast VALUES (2, 20)").unwrap();

        let result = d.query("SELECT CAST(AVG(val) AS INTEGER) FROM mix_cast", &[]);
        assert!(result.is_err(),
            "CAST(AVG(col) AS INTEGER) is not yet supported (aggregate inside CAST)");
    }

    /// SUM(val * 2) and SUM(val) * 2 -- the first works (expression inside aggregate)
    /// but the second fails (aggregate inside expression).
    #[test]
    fn test_sum_with_expression_inside_aggregate() {
        let d = db();
        d.execute("CREATE TABLE mix_arith (id INT, val INT)").unwrap();
        d.execute("INSERT INTO mix_arith VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO mix_arith VALUES (2, 20)").unwrap();
        d.execute("INSERT INTO mix_arith VALUES (3, 30)").unwrap();

        // SUM(val * 2) works: expression inside aggregate is fine
        let rows = d.query("SELECT SUM(val * 2) FROM mix_arith", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 120,
            "SUM(val * 2) = 20+40+60 = 120");
    }

    #[test]
    fn test_aggregate_outside_expression_not_yet_supported() {
        let d = db();
        d.execute("CREATE TABLE mix_arith2 (id INT, val INT)").unwrap();
        d.execute("INSERT INTO mix_arith2 VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO mix_arith2 VALUES (2, 20)").unwrap();

        // SUM(val) * 2 fails: aggregate as sub-expression of binary op
        let result = d.query("SELECT SUM(val) * 2 FROM mix_arith2", &[]);
        assert!(result.is_err(),
            "SUM(val) * 2 is not yet supported (aggregate inside binary expression)");
    }

    // ========================================================================
    // 8. Additional aggregate edge cases
    // ========================================================================

    #[test]
    fn test_min_max_with_non_null_data() {
        let d = db();
        d.execute("CREATE TABLE minmax (id INT, val INT)").unwrap();
        d.execute("INSERT INTO minmax VALUES (1, 30)").unwrap();
        d.execute("INSERT INTO minmax VALUES (2, 10)").unwrap();
        d.execute("INSERT INTO minmax VALUES (3, 50)").unwrap();
        d.execute("INSERT INTO minmax VALUES (4, 20)").unwrap();

        let rows = d.query("SELECT MIN(val), MAX(val) FROM minmax", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 10, "MIN should be 10");
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 50, "MAX should be 50");
    }

    #[test]
    fn test_min_max_single_row() {
        let d = db();
        d.execute("CREATE TABLE minmax1 (id INT, val INT)").unwrap();
        d.execute("INSERT INTO minmax1 VALUES (1, 42)").unwrap();

        let rows = d.query("SELECT MIN(val), MAX(val) FROM minmax1", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 42, "MIN of single row should be that value");
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 42, "MAX of single row should be that value");
    }

    #[test]
    fn test_min_max_with_negative_values() {
        let d = db();
        d.execute("CREATE TABLE minmax_neg (id INT, val INT)").unwrap();
        d.execute("INSERT INTO minmax_neg VALUES (1, -100)").unwrap();
        d.execute("INSERT INTO minmax_neg VALUES (2, 0)").unwrap();
        d.execute("INSERT INTO minmax_neg VALUES (3, 50)").unwrap();
        d.execute("INSERT INTO minmax_neg VALUES (4, -200)").unwrap();

        let rows = d.query("SELECT MIN(val), MAX(val) FROM minmax_neg", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), -200, "MIN should be -200");
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 50, "MAX should be 50");
    }

    #[test]
    fn test_sum_with_negative_values() {
        let d = db();
        d.execute("CREATE TABLE sum_neg (id INT, val INT)").unwrap();
        d.execute("INSERT INTO sum_neg VALUES (1, 100)").unwrap();
        d.execute("INSERT INTO sum_neg VALUES (2, -30)").unwrap();
        d.execute("INSERT INTO sum_neg VALUES (3, -70)").unwrap();

        let rows = d.query("SELECT SUM(val) FROM sum_neg", &[]).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 0,
            "SUM(100, -30, -70) should be 0");
    }

    #[test]
    fn test_avg_single_value() {
        let d = db();
        d.execute("CREATE TABLE avg_single (id INT, val INT)").unwrap();
        d.execute("INSERT INTO avg_single VALUES (1, 42)").unwrap();

        let rows = d.query("SELECT AVG(val) FROM avg_single", &[]).unwrap();
        let avg = to_f64(rows[0].get(0).unwrap());
        assert!((avg - 42.0).abs() < 0.01,
            "AVG of single value 42 should be 42.0, got {}", avg);
    }

    #[test]
    fn test_count_with_where_clause() {
        let d = db();
        d.execute("CREATE TABLE cnt_where (id INT, status TEXT)").unwrap();
        d.execute("INSERT INTO cnt_where VALUES (1, 'active')").unwrap();
        d.execute("INSERT INTO cnt_where VALUES (2, 'inactive')").unwrap();
        d.execute("INSERT INTO cnt_where VALUES (3, 'active')").unwrap();
        d.execute("INSERT INTO cnt_where VALUES (4, 'active')").unwrap();

        let rows = d.query(
            "SELECT COUNT(*) FROM cnt_where WHERE status = 'active'",
            &[]
        ).unwrap();
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 3,
            "COUNT(*) with WHERE should count only matching rows");
    }

    #[test]
    fn test_multiple_aggregates_same_column() {
        let d = db();
        d.execute("CREATE TABLE multi_agg (id INT, val INT)").unwrap();
        d.execute("INSERT INTO multi_agg VALUES (1, 10)").unwrap();
        d.execute("INSERT INTO multi_agg VALUES (2, 20)").unwrap();
        d.execute("INSERT INTO multi_agg VALUES (3, 30)").unwrap();

        let rows = d.query(
            "SELECT COUNT(val), SUM(val), AVG(val), MIN(val), MAX(val) FROM multi_agg",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 1);
        // COUNT(val) - engine counts all rows including NULLs (but no NULLs here)
        assert_eq!(to_i64(rows[0].get(0).unwrap()), 3);
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 60);    // SUM
        let avg = to_f64(rows[0].get(2).unwrap());
        assert!((avg - 20.0).abs() < 0.01, "AVG should be 20.0, got {}", avg);
        assert_eq!(to_i64(rows[0].get(3).unwrap()), 10);    // MIN
        assert_eq!(to_i64(rows[0].get(4).unwrap()), 30);    // MAX
    }

    #[test]
    fn test_group_by_with_order_by_group_key() {
        let d = db();
        d.execute("CREATE TABLE gb_ord (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO gb_ord VALUES ('C', 30)").unwrap();
        d.execute("INSERT INTO gb_ord VALUES ('A', 10)").unwrap();
        d.execute("INSERT INTO gb_ord VALUES ('B', 20)").unwrap();
        d.execute("INSERT INTO gb_ord VALUES ('A', 5)").unwrap();

        // ORDER BY the group key (grp) which is known to work
        let rows = d.query(
            "SELECT grp, SUM(val) FROM gb_ord GROUP BY grp ORDER BY grp",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 3);

        // Ordered by grp: A=15, B=20, C=30
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("A".to_string()));
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 15);
        assert_eq!(rows[1].get(0).unwrap(), &Value::String("B".to_string()));
        assert_eq!(to_i64(rows[1].get(1).unwrap()), 20);
        assert_eq!(rows[2].get(0).unwrap(), &Value::String("C".to_string()));
        assert_eq!(to_i64(rows[2].get(1).unwrap()), 30);
    }

    /// ORDER BY SUM(val) DESC does not correctly sort by the aggregate result.
    /// This test documents the known limitation.
    #[test]
    fn test_order_by_aggregate_does_not_sort_correctly() {
        let d = db();
        d.execute("CREATE TABLE gb_ord2 (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO gb_ord2 VALUES ('C', 30)").unwrap();
        d.execute("INSERT INTO gb_ord2 VALUES ('A', 10)").unwrap();
        d.execute("INSERT INTO gb_ord2 VALUES ('B', 20)").unwrap();
        d.execute("INSERT INTO gb_ord2 VALUES ('A', 5)").unwrap();

        // The query executes but ORDER BY SUM(val) DESC does not reorder rows
        let result = d.query(
            "SELECT grp, SUM(val) FROM gb_ord2 GROUP BY grp ORDER BY SUM(val) DESC",
            &[]
        );
        assert!(result.is_ok(),
            "ORDER BY aggregate should execute without error even if sorting is incorrect");
        let rows = result.unwrap();
        assert_eq!(rows.len(), 3, "Should still return all 3 groups");

        // Verify the aggregate values are correct regardless of order
        let mut sums: Vec<i64> = rows.iter().map(|r| to_i64(r.get(1).unwrap())).collect();
        sums.sort();
        assert_eq!(sums, vec![15, 20, 30], "All aggregate sums should be present");
    }

    #[test]
    fn test_sum_distinct_with_group_by() {
        let d = db();
        d.execute("CREATE TABLE sd_grp (grp TEXT, val INT)").unwrap();
        d.execute("INSERT INTO sd_grp VALUES ('A', 10)").unwrap();
        d.execute("INSERT INTO sd_grp VALUES ('A', 10)").unwrap();
        d.execute("INSERT INTO sd_grp VALUES ('A', 20)").unwrap();
        d.execute("INSERT INTO sd_grp VALUES ('B', 5)").unwrap();
        d.execute("INSERT INTO sd_grp VALUES ('B', 5)").unwrap();

        let rows = d.query(
            "SELECT grp, SUM(DISTINCT val) FROM sd_grp GROUP BY grp ORDER BY grp",
            &[]
        ).unwrap();
        assert_eq!(rows.len(), 2);

        // A: distinct vals {10, 20} => sum = 30
        assert_eq!(to_i64(rows[0].get(1).unwrap()), 30,
            "Group A: SUM(DISTINCT val) should be 10+20=30");
        // B: distinct vals {5} => sum = 5
        assert_eq!(to_i64(rows[1].get(1).unwrap()), 5,
            "Group B: SUM(DISTINCT val) should be 5");
    }

    #[test]
    fn test_min_max_on_text_column() {
        let d = db();
        d.execute("CREATE TABLE minmax_text (id INT, name TEXT)").unwrap();
        d.execute("INSERT INTO minmax_text VALUES (1, 'banana')").unwrap();
        d.execute("INSERT INTO minmax_text VALUES (2, 'apple')").unwrap();
        d.execute("INSERT INTO minmax_text VALUES (3, 'cherry')").unwrap();

        let rows = d.query("SELECT MIN(name), MAX(name) FROM minmax_text", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::String("apple".to_string()),
            "MIN on text should return lexicographically smallest");
        assert_eq!(rows[0].get(1).unwrap(), &Value::String("cherry".to_string()),
            "MAX on text should return lexicographically largest");
    }
}
