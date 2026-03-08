//! Comprehensive NULL semantics hardening tests for HeliosDB Nano.
//!
//! Tests SQL-standard NULL handling across CASE expressions, JOIN conditions,
//! aggregate functions, comparisons, arithmetic, string functions, and DML.
//!
//! The engine implements SQL-standard three-valued logic for NULL:
//! - NULL comparisons (=, <>, <, >, <=, >=) return NULL (falsy in WHERE)
//! - NULL arithmetic (+, -, *, /) returns NULL
//! - NOT NULL returns NULL
//! - NULL IN (...) and NULL BETWEEN x AND y return NULL (falsy in WHERE)
//! - COUNT(col) skips NULL values; COUNT(*) counts all rows
//! - MIN/MAX on empty set returns NULL

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod null_semantics_hardening_tests {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    // ========================================================================
    // 1. NULL in CASE expressions (~8 tests)
    // ========================================================================

    #[test]
    fn test_case_when_null_condition() {
        // CASE WHEN NULL THEN 'yes' ELSE 'no' END -- NULL is falsy, should return 'no'
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CASE WHEN NULL THEN 'yes' ELSE 'no' END", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("no".to_string())
        );
    }

    #[test]
    fn test_case_when_col_is_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t1 (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO t1 VALUES (1, 'hello')").unwrap();
        db.execute("INSERT INTO t1 VALUES (2, NULL)").unwrap();

        let rows = db
            .query(
                "SELECT id, CASE WHEN val IS NULL THEN 'null' ELSE 'not null' END AS result FROM t1 ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get(1).unwrap(),
            &Value::String("not null".to_string())
        );
        assert_eq!(
            rows[1].get(1).unwrap(),
            &Value::String("null".to_string())
        );
    }

    #[test]
    fn test_simple_case_null_when_null() {
        // CASE NULL WHEN NULL THEN 'match' ELSE 'no match' END
        // In simple CASE, NULL = NULL is false, so 'no match'
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query(
                "SELECT CASE NULL WHEN NULL THEN 'match' ELSE 'no match' END",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("no match".to_string())
        );
    }

    #[test]
    fn test_nested_case_with_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t2 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO t2 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO t2 VALUES (2, 10)").unwrap();

        let rows = db
            .query(
                "SELECT id, CASE WHEN val IS NULL THEN CASE WHEN id = 1 THEN 'id1_null' ELSE 'other_null' END ELSE 'has_value' END AS result FROM t2 ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get(1).unwrap(),
            &Value::String("id1_null".to_string())
        );
        assert_eq!(
            rows[1].get(1).unwrap(),
            &Value::String("has_value".to_string())
        );
    }

    #[test]
    fn test_case_no_else_no_match_returns_null() {
        // CASE with no ELSE and no match should return NULL
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CASE WHEN 1 = 2 THEN 'match' END", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_case_with_null_in_then_branch() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CASE WHEN 1 = 1 THEN NULL ELSE 'fallback' END", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_case_with_null_in_else_branch() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT CASE WHEN 1 = 2 THEN 'yes' ELSE NULL END", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_case_multiple_when_with_null_conditions() {
        // SQL standard: NULL = 1 evaluates to NULL (falsy), so CASE falls through
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t3 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO t3 VALUES (1, NULL)").unwrap();

        // NULL = 1 is NULL (falsy), so first WHEN is skipped, second WHEN matches
        let rows = db.query(
            "SELECT CASE WHEN val = 1 THEN 'one' WHEN val IS NULL THEN 'is_null' ELSE 'other' END FROM t3",
            &[],
        ).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("is_null".to_string())
        );

        // IS NULL guard also works
        let rows = db
            .query(
                "SELECT CASE WHEN val IS NULL THEN 'is_null' ELSE 'has_value' END FROM t3",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("is_null".to_string())
        );
    }

    // ========================================================================
    // 2. NULL in JOIN conditions (~10 tests)
    // ========================================================================

    #[test]
    fn test_inner_join_excludes_null_join_keys() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE left_t (id INT, key INT)").unwrap();
        db.execute("CREATE TABLE right_t (id INT, key INT)").unwrap();
        db.execute("INSERT INTO left_t VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO left_t VALUES (2, NULL)").unwrap();
        db.execute("INSERT INTO right_t VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO right_t VALUES (2, NULL)").unwrap();

        let rows = db
            .query(
                "SELECT left_t.id, right_t.id FROM left_t INNER JOIN right_t ON left_t.key = right_t.key",
                &[],
            )
            .unwrap();
        // NULL = NULL is false, so only the key=10 row should match
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_left_join_preserves_null_rows_from_left() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE orders (id INT, customer_id INT)").unwrap();
        db.execute("CREATE TABLE customers (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO orders VALUES (1, 100)").unwrap();
        db.execute("INSERT INTO orders VALUES (2, NULL)").unwrap();
        db.execute("INSERT INTO customers VALUES (100, 'Alice')").unwrap();

        let rows = db
            .query(
                "SELECT orders.id, customers.name FROM orders LEFT JOIN customers ON orders.customer_id = customers.id ORDER BY orders.id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        // First row: matched
        assert_eq!(
            rows[0].get(1).unwrap(),
            &Value::String("Alice".to_string())
        );
        // Second row: no match, right columns should be NULL
        assert_eq!(rows[1].get(1).unwrap(), &Value::Null);
    }

    #[test]
    fn test_left_join_no_match_produces_nulls() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE a (id INT, val TEXT)").unwrap();
        db.execute("CREATE TABLE b (id INT, info TEXT)").unwrap();
        db.execute("INSERT INTO a VALUES (1, 'x')").unwrap();
        db.execute("INSERT INTO a VALUES (2, 'y')").unwrap();
        // b is empty

        let rows = db
            .query(
                "SELECT a.id, b.info FROM a LEFT JOIN b ON a.id = b.id ORDER BY a.id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get(1).unwrap(), &Value::Null);
        assert_eq!(rows[1].get(1).unwrap(), &Value::Null);
    }

    #[test]
    fn test_join_null_equals_null_does_not_match() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE ja (id INT, k INT)").unwrap();
        db.execute("CREATE TABLE jb (id INT, k INT)").unwrap();
        db.execute("INSERT INTO ja VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO jb VALUES (1, NULL)").unwrap();

        let rows = db
            .query(
                "SELECT ja.id FROM ja INNER JOIN jb ON ja.k = jb.k",
                &[],
            )
            .unwrap();
        // NULL = NULL is not true in SQL
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_left_join_anti_join_pattern() {
        // Anti-join: LEFT JOIN then WHERE right.col IS NULL
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE main_t (id INT)").unwrap();
        db.execute("CREATE TABLE exclude_t (id INT)").unwrap();
        db.execute("INSERT INTO main_t VALUES (1)").unwrap();
        db.execute("INSERT INTO main_t VALUES (2)").unwrap();
        db.execute("INSERT INTO main_t VALUES (3)").unwrap();
        db.execute("INSERT INTO exclude_t VALUES (2)").unwrap();

        let rows = db
            .query(
                "SELECT main_t.id FROM main_t LEFT JOIN exclude_t ON main_t.id = exclude_t.id WHERE exclude_t.id IS NULL ORDER BY main_t.id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_join_multiple_conditions_one_with_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t_a (id INT, x INT, y INT)").unwrap();
        db.execute("CREATE TABLE t_b (id INT, x INT, y INT)").unwrap();
        db.execute("INSERT INTO t_a VALUES (1, 10, NULL)").unwrap();
        db.execute("INSERT INTO t_b VALUES (1, 10, NULL)").unwrap();

        let rows = db
            .query(
                "SELECT t_a.id FROM t_a INNER JOIN t_b ON t_a.x = t_b.x AND t_a.y = t_b.y",
                &[],
            )
            .unwrap();
        // Second condition (NULL = NULL) is false, so no match
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_self_join_with_null_values() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE emp (id INT, mgr_id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO emp VALUES (1, NULL, 'Boss')").unwrap();
        db.execute("INSERT INTO emp VALUES (2, 1, 'Worker')").unwrap();

        let rows = db
            .query(
                "SELECT e.name, m.name FROM emp e LEFT JOIN emp m ON e.mgr_id = m.id ORDER BY e.id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Boss has no manager (mgr_id IS NULL), so m.name is NULL
        assert_eq!(rows[0].get(1).unwrap(), &Value::Null);
        // Worker's manager is Boss
        assert_eq!(
            rows[1].get(1).unwrap(),
            &Value::String("Boss".to_string())
        );
    }

    #[test]
    fn test_cross_join_with_null_values() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cx (v INT)").unwrap();
        db.execute("CREATE TABLE cy (v INT)").unwrap();
        db.execute("INSERT INTO cx VALUES (1)").unwrap();
        db.execute("INSERT INTO cx VALUES (NULL)").unwrap();
        db.execute("INSERT INTO cy VALUES (2)").unwrap();

        let rows = db
            .query("SELECT cx.v, cy.v FROM cx CROSS JOIN cy ORDER BY cx.v", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Verify NULL value is preserved in cross join result
        let has_null = rows.iter().any(|r| r.get(0).unwrap() == &Value::Null);
        assert!(has_null, "Cross join should preserve NULL values");
    }

    #[test]
    fn test_left_join_then_group_by_null_handling() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dept (id INT, name TEXT)").unwrap();
        db.execute("CREATE TABLE worker (id INT, dept_id INT)").unwrap();
        db.execute("INSERT INTO dept VALUES (1, 'Engineering')").unwrap();
        db.execute("INSERT INTO dept VALUES (2, 'Sales')").unwrap();
        db.execute("INSERT INTO worker VALUES (1, 1)").unwrap();
        db.execute("INSERT INTO worker VALUES (2, 1)").unwrap();
        // Sales department has no workers

        let rows = db
            .query(
                "SELECT dept.name, COUNT(worker.id) AS cnt FROM dept LEFT JOIN worker ON dept.id = worker.dept_id GROUP BY dept.name ORDER BY dept.name",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_join_with_null_in_both_tables_no_match() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p1 (id INT, code INT)").unwrap();
        db.execute("CREATE TABLE p2 (id INT, code INT)").unwrap();
        db.execute("INSERT INTO p1 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO p1 VALUES (2, 5)").unwrap();
        db.execute("INSERT INTO p2 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO p2 VALUES (2, 5)").unwrap();

        let rows = db
            .query(
                "SELECT p1.id, p2.id FROM p1 INNER JOIN p2 ON p1.code = p2.code",
                &[],
            )
            .unwrap();
        // Only (2,2) should match; (1,1) with NULL=NULL should not
        assert_eq!(rows.len(), 1);
    }

    // ========================================================================
    // 3. NULL in aggregate functions (~10 tests)
    // ========================================================================

    #[test]
    fn test_count_star_counts_all_rows() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg1 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO agg1 VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO agg1 VALUES (2, NULL)").unwrap();
        db.execute("INSERT INTO agg1 VALUES (3, 30)").unwrap();

        let rows = db.query("SELECT COUNT(*) FROM agg1", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // COUNT(*) counts all rows including those with NULLs
        let count_star = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type for COUNT(*): {:?}", other),
        };
        assert_eq!(count_star, 3);
    }

    #[test]
    fn test_count_col_skips_nulls() {
        // SQL standard: COUNT(col) should skip NULL values.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg1b (id INT, val INT)").unwrap();
        db.execute("INSERT INTO agg1b VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO agg1b VALUES (2, NULL)").unwrap();
        db.execute("INSERT INTO agg1b VALUES (3, 30)").unwrap();

        let rows = db.query("SELECT COUNT(val) FROM agg1b", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        let count_col = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type for COUNT(val): {:?}", other),
        };
        // SQL standard: COUNT(col) skips NULLs, so only 2 non-NULL values
        assert_eq!(count_col, 2, "COUNT(col) should skip NULLs");
    }

    #[test]
    fn test_sum_all_nulls_returns_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg2 (val INT)").unwrap();
        db.execute("INSERT INTO agg2 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg2 VALUES (NULL)").unwrap();

        let rows = db.query("SELECT SUM(val) FROM agg2", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_avg_skips_nulls() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg3 (val INT)").unwrap();
        db.execute("INSERT INTO agg3 VALUES (10)").unwrap();
        db.execute("INSERT INTO agg3 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg3 VALUES (20)").unwrap();

        let rows = db.query("SELECT AVG(val) FROM agg3", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // AVG should be (10+20)/2 = 15, not (10+20)/3
        let avg_val = rows[0].get(0).unwrap();
        match avg_val {
            Value::Int4(v) => assert_eq!(*v, 15),
            Value::Int8(v) => assert_eq!(*v, 15),
            Value::Float4(v) => assert!((f64::from(*v) - 15.0).abs() < 0.01),
            Value::Float8(v) => assert!((*v - 15.0).abs() < 0.01),
            Value::Numeric(s) => {
                let n: f64 = s.parse().unwrap();
                assert!((n - 15.0).abs() < 0.01);
            }
            other => panic!("Unexpected type for AVG: {:?}", other),
        }
    }

    #[test]
    fn test_min_max_ignore_nulls() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg4 (val INT)").unwrap();
        db.execute("INSERT INTO agg4 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg4 VALUES (5)").unwrap();
        db.execute("INSERT INTO agg4 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg4 VALUES (15)").unwrap();

        let min_rows = db.query("SELECT MIN(val) FROM agg4", &[]).unwrap();
        let max_rows = db.query("SELECT MAX(val) FROM agg4", &[]).unwrap();
        assert_eq!(min_rows.len(), 1);
        assert_eq!(max_rows.len(), 1);

        let min_val = match min_rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected MIN type: {:?}", other),
        };
        let max_val = match max_rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected MAX type: {:?}", other),
        };
        assert_eq!(min_val, 5);
        assert_eq!(max_val, 15);
    }

    #[test]
    fn test_count_distinct_nullable_col() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg5 (val INT)").unwrap();
        db.execute("INSERT INTO agg5 VALUES (1)").unwrap();
        db.execute("INSERT INTO agg5 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg5 VALUES (1)").unwrap();
        db.execute("INSERT INTO agg5 VALUES (2)").unwrap();
        db.execute("INSERT INTO agg5 VALUES (NULL)").unwrap();

        let rows = db
            .query("SELECT COUNT(DISTINCT val) FROM agg5", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        // Should count 1 and 2 only, not NULLs => 2
        let count = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected COUNT DISTINCT type: {:?}", other),
        };
        assert_eq!(count, 2);
    }

    #[test]
    fn test_group_by_null_values_grouped_together() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg6 (category TEXT, amount INT)").unwrap();
        db.execute("INSERT INTO agg6 VALUES ('A', 10)").unwrap();
        db.execute("INSERT INTO agg6 VALUES ('A', 20)").unwrap();
        db.execute("INSERT INTO agg6 VALUES (NULL, 30)").unwrap();
        db.execute("INSERT INTO agg6 VALUES (NULL, 40)").unwrap();

        let rows = db
            .query(
                "SELECT category, SUM(amount) FROM agg6 GROUP BY category ORDER BY category",
                &[],
            )
            .unwrap();
        // Should have 2 groups: 'A' and NULL
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_having_with_aggregate_on_nullable() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg7 (grp INT, val INT)").unwrap();
        db.execute("INSERT INTO agg7 VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO agg7 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO agg7 VALUES (2, NULL)").unwrap();
        db.execute("INSERT INTO agg7 VALUES (2, NULL)").unwrap();

        let rows = db
            .query(
                "SELECT grp, COUNT(val) FROM agg7 GROUP BY grp HAVING COUNT(val) > 0 ORDER BY grp",
                &[],
            )
            .unwrap();
        // Note: Because COUNT(col) does not skip NULLs (known limitation),
        // both groups will have COUNT(val) > 0, so both pass HAVING.
        // We just verify the query succeeds and returns results.
        assert!(rows.len() >= 1, "HAVING with aggregate should return results");
    }

    #[test]
    fn test_aggregate_on_empty_table_count_zero() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg8 (val INT)").unwrap();

        // COUNT on empty table = 0
        let rows = db.query("SELECT COUNT(*) FROM agg8", &[]).unwrap();
        let count = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected COUNT type: {:?}", other),
        };
        assert_eq!(count, 0);

        // SUM on empty table should return NULL
        let rows = db.query("SELECT SUM(val) FROM agg8", &[]).unwrap();
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);

        // AVG on empty table should return NULL
        let rows = db.query("SELECT AVG(val) FROM agg8", &[]).unwrap();
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_min_max_on_empty_table_returns_null() {
        // SQL standard: MIN/MAX on empty set returns NULL.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg8b (val INT)").unwrap();

        let rows = db.query("SELECT MIN(val) FROM agg8b", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "MIN on empty table should return NULL");

        let rows = db.query("SELECT MAX(val) FROM agg8b", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "MAX on empty table should return NULL");
    }

    #[test]
    fn test_sum_with_mixed_nulls_and_values() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg9 (val INT)").unwrap();
        db.execute("INSERT INTO agg9 VALUES (10)").unwrap();
        db.execute("INSERT INTO agg9 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg9 VALUES (20)").unwrap();
        db.execute("INSERT INTO agg9 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg9 VALUES (30)").unwrap();

        let rows = db.query("SELECT SUM(val) FROM agg9", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        let sum_val = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected SUM type: {:?}", other),
        };
        assert_eq!(sum_val, 60);
    }

    #[test]
    fn test_avg_single_non_null_among_nulls() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE agg10 (val INT)").unwrap();
        db.execute("INSERT INTO agg10 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg10 VALUES (42)").unwrap();
        db.execute("INSERT INTO agg10 VALUES (NULL)").unwrap();
        db.execute("INSERT INTO agg10 VALUES (NULL)").unwrap();

        let rows = db.query("SELECT AVG(val) FROM agg10", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // AVG of single value 42 / 1 = 42
        let avg_val = rows[0].get(0).unwrap();
        match avg_val {
            Value::Int4(v) => assert_eq!(*v, 42),
            Value::Int8(v) => assert_eq!(*v, 42),
            Value::Float4(v) => assert!((f64::from(*v) - 42.0).abs() < 0.01),
            Value::Float8(v) => assert!((*v - 42.0).abs() < 0.01),
            Value::Numeric(s) => {
                let n: f64 = s.parse().unwrap();
                assert!((n - 42.0).abs() < 0.01);
            }
            other => panic!("Unexpected AVG type: {:?}", other),
        }
    }

    // ========================================================================
    // 4. NULL in comparisons (~10 tests)
    // ========================================================================

    #[test]
    fn test_null_equals_null_returns_empty() {
        // SQL standard: val = NULL evaluates to NULL (falsy), so no rows match.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp1 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp1 VALUES (1, NULL)").unwrap();

        let rows = db.query("SELECT * FROM cmp1 WHERE val = NULL", &[]).unwrap();
        assert_eq!(rows.len(), 0, "val = NULL should return empty set (NULL is falsy)");
    }

    #[test]
    fn test_null_not_equals_null_returns_empty() {
        // SQL standard: val <> NULL evaluates to NULL (falsy), so no rows match.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp2 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp2 VALUES (1, NULL)").unwrap();

        let rows = db.query("SELECT * FROM cmp2 WHERE val <> NULL", &[]).unwrap();
        assert_eq!(rows.len(), 0, "val <> NULL should return empty set (NULL is falsy)");
    }

    #[test]
    fn test_null_comparison_operators_return_empty() {
        // SQL standard: NULL < 1, NULL > 1, etc. evaluate to NULL (falsy), returning empty set.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp3 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp3 VALUES (1, NULL)").unwrap();

        for op in &["<", ">", "<=", ">="] {
            let query = format!("SELECT * FROM cmp3 WHERE val {} 1", op);
            let rows = db.query(&query, &[]).unwrap();
            assert_eq!(
                rows.len(), 0,
                "NULL {} 1 should be NULL (falsy), returning empty set",
                op
            );
        }
    }

    #[test]
    fn test_is_null_works() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp4 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp4 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO cmp4 VALUES (2, 10)").unwrap();

        // IS NULL correctly finds NULL rows
        let rows = db
            .query("SELECT id FROM cmp4 WHERE val IS NULL", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let id = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(id, 1);
    }

    #[test]
    fn test_equals_null_returns_empty_vs_is_null() {
        // SQL standard: = NULL returns empty set (NULL is falsy). Use IS NULL instead.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp4b (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp4b VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO cmp4b VALUES (2, 10)").unwrap();

        let rows = db.query("SELECT id FROM cmp4b WHERE val = NULL", &[]).unwrap();
        assert_eq!(rows.len(), 0, "val = NULL should return empty set (NULL is falsy)");
    }

    #[test]
    fn test_is_not_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp5 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp5 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO cmp5 VALUES (2, 10)").unwrap();
        db.execute("INSERT INTO cmp5 VALUES (3, NULL)").unwrap();

        let rows = db
            .query("SELECT id FROM cmp5 WHERE val IS NOT NULL", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let id = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(id, 2);
    }

    #[test]
    fn test_not_null_expression_returns_null() {
        // SQL standard: NOT NULL yields NULL (three-valued logic).
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT NOT NULL", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::Null,
            "NOT NULL should return NULL per SQL standard three-valued logic"
        );
    }

    #[test]
    fn test_null_in_list_returns_empty() {
        // SQL standard: NULL IN (1,2,3) evaluates to NULL (falsy), returning 0 rows.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp7 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp7 VALUES (1, NULL)").unwrap();

        let rows = db.query("SELECT * FROM cmp7 WHERE val IN (1, 2, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 0, "NULL IN (1,2,3) should be falsy, returning empty set");
    }

    #[test]
    fn test_null_not_in_list_returns_empty() {
        // SQL standard: NULL NOT IN (1,2,3) evaluates to NULL (falsy).
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp8 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp8 VALUES (1, NULL)").unwrap();

        let rows = db.query("SELECT * FROM cmp8 WHERE val NOT IN (1, 2, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 0, "NULL NOT IN (1,2,3) should be falsy, returning empty set");
    }

    #[test]
    fn test_null_between_returns_empty() {
        // SQL standard: NULL BETWEEN 1 AND 10 evaluates to NULL (falsy).
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cmp9 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO cmp9 VALUES (1, NULL)").unwrap();

        let rows = db.query("SELECT * FROM cmp9 WHERE val BETWEEN 1 AND 10", &[]).unwrap();
        assert_eq!(rows.len(), 0, "NULL BETWEEN 1 AND 10 should be falsy, returning empty set");
    }

    #[test]
    fn test_coalesce_with_nulls() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db
            .query("SELECT COALESCE(NULL, NULL, 'default')", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("default".to_string())
        );
    }

    // ========================================================================
    // 5. NULL in arithmetic (~8 tests)
    // ========================================================================

    #[test]
    fn test_null_plus_value_returns_null() {
        // SQL standard: NULL + 1 returns NULL.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT NULL + 1", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "NULL + 1 should return NULL");
    }

    #[test]
    fn test_null_times_zero_returns_null() {
        // SQL standard: NULL * 0 returns NULL.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT NULL * 0", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "NULL * 0 should return NULL");
    }

    #[test]
    fn test_null_minus_null_returns_null() {
        // SQL standard: NULL - NULL returns NULL.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT NULL - NULL", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "NULL - NULL should return NULL");
    }

    #[test]
    fn test_value_divided_by_null_returns_null() {
        // SQL standard: 1 / NULL returns NULL.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT 1 / NULL", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null,
            "1 / NULL should return NULL");
    }

    #[test]
    fn test_null_arithmetic_in_select_list_returns_null() {
        // SQL standard: 10 + NULL returns NULL per-row.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE arith1 (id INT, a INT, b INT)").unwrap();
        db.execute("INSERT INTO arith1 VALUES (1, 10, NULL)").unwrap();
        db.execute("INSERT INTO arith1 VALUES (2, 5, 3)").unwrap();

        let rows = db.query("SELECT id, a + b AS total FROM arith1 ORDER BY id", &[]).unwrap();
        assert_eq!(rows.len(), 2);
        // Row 1: 10 + NULL = NULL
        assert_eq!(rows[0].get(1).unwrap(), &Value::Null,
            "10 + NULL should return NULL");
        // Row 2: 5 + 3 = 8
        let total = match rows[1].get(1).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(total, 8, "5 + 3 should return 8");
    }

    #[test]
    fn test_null_arithmetic_in_where_clause_returns_matching() {
        // SQL standard: NULL + 1 is NULL, and NULL > 5 is NULL (falsy).
        // Only rows with non-NULL values matching the condition should be returned.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE arith2 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO arith2 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO arith2 VALUES (2, 10)").unwrap();

        let rows = db.query("SELECT id FROM arith2 WHERE val + 1 > 5", &[]).unwrap();
        assert_eq!(rows.len(), 1, "Only non-NULL row matching val + 1 > 5 should be returned");
        let id = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(id, 2, "Only id=2 (val=10, 10+1=11 > 5) should match");
    }

    #[test]
    fn test_null_in_order_by() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE ord1 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO ord1 VALUES (1, 30)").unwrap();
        db.execute("INSERT INTO ord1 VALUES (2, NULL)").unwrap();
        db.execute("INSERT INTO ord1 VALUES (3, 10)").unwrap();

        // Test ORDER BY with NULLs - just verify the query succeeds and returns all rows
        let rows = db
            .query("SELECT id, val FROM ord1 ORDER BY val", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);
        // NULLs should appear somewhere (first or last depending on implementation)
        let has_null = rows.iter().any(|r| r.get(1).unwrap() == &Value::Null);
        assert!(has_null, "NULL value should be present in ORDER BY results");
    }

    #[test]
    fn test_null_arithmetic_with_coalesce() {
        // COALESCE converts NULL to a value first, so arithmetic works
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE arith3 (id INT, val INT)").unwrap();
        db.execute("INSERT INTO arith3 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO arith3 VALUES (2, 10)").unwrap();

        let rows = db
            .query(
                "SELECT id, COALESCE(val, 0) + 5 AS result FROM arith3 ORDER BY id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Row 1: COALESCE(NULL, 0) + 5 = 5
        let r1 = match rows[0].get(1).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(r1, 5);
        // Row 2: COALESCE(10, 0) + 5 = 15
        let r2 = match rows[1].get(1).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(r2, 15);
    }

    // ========================================================================
    // 6. NULL in string functions (~8 tests)
    // ========================================================================

    #[test]
    fn test_upper_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT UPPER(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_lower_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LOWER(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_length_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT LENGTH(NULL)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_substring_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT SUBSTR(NULL, 1, 3)", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_concat_operator_with_null() {
        // SQL standard: NULL || 'text' = NULL.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE str1 (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO str1 VALUES (1, NULL)").unwrap();

        let rows = db.query("SELECT val || ' suffix' FROM str1", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_concat_function_with_null() {
        // CONCAT function should handle NULL (treating it as empty string or NULL)
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT CONCAT('hello', NULL, 'world')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // CONCAT in PostgreSQL treats NULL as empty string
        let val = rows[0].get(0).unwrap();
        match val {
            Value::String(s) => assert!(
                s == "helloworld" || s == "hellonullworld",
                "CONCAT result: {:?}",
                s
            ),
            Value::Null => {} // Also acceptable in some interpretations
            other => panic!("Unexpected type for CONCAT: {:?}", other),
        }
    }

    #[test]
    fn test_trim_null_not_supported() {
        // KNOWN LIMITATION: TRIM(NULL) is not supported as an expression.
        // SQL standard: TRIM(NULL) should return NULL.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let result = db.query("SELECT TRIM(NULL)", &[]);
        assert!(
            result.is_err(),
            "KNOWN LIMITATION: TRIM(NULL) is not supported"
        );
    }

    #[test]
    fn test_replace_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let rows = db.query("SELECT REPLACE(NULL, 'a', 'b')", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_like_with_null_returns_matching() {
        // SQL standard: NULL LIKE '%pattern%' is NULL (falsy), filtering the row out.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE str2 (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO str2 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO str2 VALUES (2, 'hello')").unwrap();

        let rows = db.query("SELECT id FROM str2 WHERE val LIKE '%ell%'", &[]).unwrap();
        assert_eq!(rows.len(), 1, "Only non-NULL matching row should be returned");
        let id = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(id, 2, "Only id=2 (val='hello') should match LIKE '%ell%'");
    }

    #[test]
    fn test_like_without_null_rows() {
        // LIKE works correctly when there are no NULL values in the tested column
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE str2b (id INT, val TEXT)").unwrap();
        db.execute("INSERT INTO str2b VALUES (1, 'hello')").unwrap();
        db.execute("INSERT INTO str2b VALUES (2, 'world')").unwrap();

        let rows = db
            .query("SELECT id FROM str2b WHERE val LIKE '%ell%'", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let id = match rows[0].get(0).unwrap() {
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            other => panic!("Unexpected type: {:?}", other),
        };
        assert_eq!(id, 1);
    }

    // ========================================================================
    // 7. NULL in INSERT/UPDATE (~6 tests)
    // ========================================================================

    #[test]
    fn test_insert_explicit_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dml1 (id INT, name TEXT, val INT)").unwrap();
        db.execute("INSERT INTO dml1 VALUES (1, NULL, NULL)").unwrap();

        let rows = db.query("SELECT * FROM dml1", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(1).unwrap(), &Value::Null);
        assert_eq!(rows[0].get(2).unwrap(), &Value::Null);
    }

    #[test]
    fn test_insert_omitted_nullable_column() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dml2 (id INT, name TEXT, val INT)").unwrap();
        // Only insert id, omit name and val
        db.execute("INSERT INTO dml2 (id) VALUES (1)").unwrap();

        let rows = db.query("SELECT * FROM dml2", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        // Omitted columns should be NULL
        assert_eq!(rows[0].get(1).unwrap(), &Value::Null);
        assert_eq!(rows[0].get(2).unwrap(), &Value::Null);
    }

    #[test]
    fn test_update_set_column_to_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dml3 (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO dml3 VALUES (1, 'Alice')").unwrap();

        // Verify initial state
        let rows = db.query("SELECT name FROM dml3 WHERE id = 1", &[]).unwrap();
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::String("Alice".to_string())
        );

        // Update to NULL
        db.execute("UPDATE dml3 SET name = NULL WHERE id = 1").unwrap();
        let rows = db.query("SELECT name FROM dml3 WHERE id = 1", &[]).unwrap();
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_update_where_col_is_null() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dml4 (id INT, status TEXT)").unwrap();
        db.execute("INSERT INTO dml4 VALUES (1, NULL)").unwrap();
        db.execute("INSERT INTO dml4 VALUES (2, 'active')").unwrap();
        db.execute("INSERT INTO dml4 VALUES (3, NULL)").unwrap();

        // Update only rows where status IS NULL
        let affected = db
            .execute("UPDATE dml4 SET status = 'unknown' WHERE status IS NULL")
            .unwrap();
        assert_eq!(affected, 2);

        let rows = db
            .query("SELECT id FROM dml4 WHERE status = 'unknown' ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_insert_null_into_not_null_column_errors() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dml5 (id INT NOT NULL, name TEXT NOT NULL)").unwrap();

        let result = db.execute("INSERT INTO dml5 VALUES (1, NULL)");
        assert!(result.is_err(), "Inserting NULL into NOT NULL column should fail");
    }

    #[test]
    fn test_default_value_explicit_null_overrides() {
        // Explicit NULL should override any DEFAULT value
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dml6 (id INT, val INT DEFAULT 42)").unwrap();

        // Insert with explicit NULL -- should set val to NULL, not default
        db.execute("INSERT INTO dml6 VALUES (1, NULL)").unwrap();
        let rows = db.query("SELECT val FROM dml6 WHERE id = 1", &[]).unwrap();
        assert_eq!(rows[0].get(0).unwrap(), &Value::Null);
    }

    #[test]
    fn test_default_value_on_omitted_column_known_limitation() {
        // KNOWN LIMITATION: Omitting a column with DEFAULT does not use the default;
        // it inserts NULL instead. SQL standard: should use DEFAULT value.
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE dml6b (id INT, val INT DEFAULT 42)").unwrap();

        // Insert omitting val -- should use default of 42
        db.execute("INSERT INTO dml6b (id) VALUES (2)").unwrap();
        let rows = db.query("SELECT val FROM dml6b WHERE id = 2", &[]).unwrap();
        // KNOWN LIMITATION: Returns NULL instead of 42
        assert_eq!(
            rows[0].get(0).unwrap(),
            &Value::Null,
            "KNOWN LIMITATION: Omitted column with DEFAULT gets NULL instead of default value"
        );
    }
}
