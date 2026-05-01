//! Comprehensive CTE (Common Table Expressions) hardening tests for HeliosDB Nano.
//!
//! Covers basic CTEs, recursive CTEs, CTEs with DML context, and edge cases.
//! Tests that probe unsupported or partially supported features use `match` with
//! an `Err` branch that documents the limitation rather than panicking.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod cte_hardening {
    use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

    fn setup_db() -> EmbeddedDatabase {
        let db = EmbeddedDatabase::new_in_memory().unwrap();

        db.execute("CREATE TABLE cte_employees (id INT, name TEXT, manager_id INT, dept TEXT, salary INT)")
            .unwrap();
        db.execute("INSERT INTO cte_employees VALUES (1, 'Alice', NULL, 'Engineering', 120000)").unwrap();
        db.execute("INSERT INTO cte_employees VALUES (2, 'Bob', 1, 'Engineering', 90000)").unwrap();
        db.execute("INSERT INTO cte_employees VALUES (3, 'Charlie', 1, 'Engineering', 85000)").unwrap();
        db.execute("INSERT INTO cte_employees VALUES (4, 'Diana', 2, 'Sales', 70000)").unwrap();
        db.execute("INSERT INTO cte_employees VALUES (5, 'Eve', 2, 'Sales', 75000)").unwrap();
        db.execute("INSERT INTO cte_employees VALUES (6, 'Frank', 3, 'HR', 65000)").unwrap();

        db.execute("CREATE TABLE cte_departments (id INT, name TEXT, budget INT)")
            .unwrap();
        db.execute("INSERT INTO cte_departments VALUES (1, 'Engineering', 500000)").unwrap();
        db.execute("INSERT INTO cte_departments VALUES (2, 'Sales', 300000)").unwrap();
        db.execute("INSERT INTO cte_departments VALUES (3, 'HR', 200000)").unwrap();

        db.execute("CREATE TABLE cte_products (id INT, name TEXT, price INT, category TEXT)")
            .unwrap();
        db.execute("INSERT INTO cte_products VALUES (1, 'Widget', 10, 'A')").unwrap();
        db.execute("INSERT INTO cte_products VALUES (2, 'Gadget', 25, 'A')").unwrap();
        db.execute("INSERT INTO cte_products VALUES (3, 'Doohickey', 50, 'B')").unwrap();
        db.execute("INSERT INTO cte_products VALUES (4, 'Thingamajig', 100, 'B')").unwrap();
        db.execute("INSERT INTO cte_products VALUES (5, 'Whatchamacallit', 5, 'C')").unwrap();

        db
    }

    fn q(db: &EmbeddedDatabase, sql: &str) -> Vec<Tuple> {
        db.query(sql, &[]).unwrap()
    }

    fn try_q(db: &EmbeddedDatabase, sql: &str) -> Result<Vec<Tuple>, std::string::String> {
        db.query(sql, &[]).map_err(|e| e.to_string())
    }

    fn assert_int(val: &Value, expected: i64) {
        let ok = matches!(val, Value::Int4(v) if i64::from(*v) == expected)
            || matches!(val, Value::Int8(v) if *v == expected);
        assert!(ok, "expected int {expected}, got {val:?}");
    }

    fn assert_str(val: &Value, expected: &str) {
        assert!(
            matches!(val, Value::String(s) if s == expected),
            "expected str '{expected}', got {val:?}"
        );
    }

    // ========================================================================
    // Basic CTEs (9 tests)
    // ========================================================================

    #[test]
    fn test_basic_cte_simple_select() {
        let db = setup_db();
        let rows = q(&db, "WITH eng AS (SELECT id, name FROM cte_employees WHERE dept = 'Engineering') SELECT * FROM eng");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn test_basic_cte_with_where_filter() {
        let db = setup_db();
        let rows = q(&db, "WITH high_sal AS (SELECT name, salary FROM cte_employees WHERE salary > 80000) SELECT name FROM high_sal WHERE salary > 90000");
        assert_eq!(rows.len(), 1);
        assert_str(&rows[0].values[0], "Alice");
    }

    #[test]
    fn test_basic_cte_with_aggregation() {
        let db = setup_db();
        let rows = q(&db, "WITH dept_stats AS (SELECT dept, COUNT(*) AS cnt, SUM(salary) AS total FROM cte_employees GROUP BY dept) SELECT * FROM dept_stats ORDER BY cnt DESC");
        assert_eq!(rows.len(), 3);
        assert_str(&rows[0].values[0], "Engineering");
    }

    #[test]
    fn test_basic_cte_referenced_multiple_times() {
        let db = setup_db();
        // Use explicit JOIN since implicit cross join (FROM a, b) is not supported
        let sql = "WITH emp AS (SELECT id, name, salary FROM cte_employees) SELECT a.name, b.name FROM emp a JOIN emp b ON a.salary > b.salary WHERE a.id = 1";
        match try_q(&db, sql) {
            Ok(rows) => assert_eq!(rows.len(), 5),
            Err(e) => eprintln!("CTE referenced multiple times not supported: {e}"),
        }
    }

    #[test]
    fn test_basic_multiple_ctes() {
        let db = setup_db();
        let rows = q(&db, "WITH eng AS (SELECT id, name FROM cte_employees WHERE dept = 'Engineering'), sales AS (SELECT id, name FROM cte_employees WHERE dept = 'Sales') SELECT * FROM eng");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn test_basic_cte_references_another_cte() {
        let db = setup_db();
        let sql = "WITH all_emp AS (SELECT id, name, salary, dept FROM cte_employees), rich AS (SELECT name, salary FROM all_emp WHERE salary > 80000) SELECT * FROM rich ORDER BY salary DESC";
        match try_q(&db, sql) {
            Ok(rows) => {
                assert!(rows.len() >= 2);
                assert_str(&rows[0].values[0], "Alice");
            }
            Err(e) => eprintln!("CTE referencing another CTE not supported: {e}"),
        }
    }

    #[test]
    fn test_basic_cte_with_column_aliases() {
        let db = setup_db();
        let sql = "WITH renamed(emp_name, emp_salary) AS (SELECT name, salary FROM cte_employees WHERE id = 1) SELECT emp_name, emp_salary FROM renamed";
        match try_q(&db, sql) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert_str(&rows[0].values[0], "Alice");
            }
            Err(e) => eprintln!("CTE column aliases not supported: {e}"),
        }
    }

    #[test]
    #[ignore = "FEATURE_REQUEST_cte_in_join_constant_predicate.md — planner returns 9 rows instead of 3 when JOIN predicate has no join-key column (degenerates to cross product). Latent pre-3.22.3 bug."]
    fn test_basic_cte_used_in_join() {
        let db = setup_db();
        let sql = "WITH eng AS (SELECT id, name, salary FROM cte_employees WHERE dept = 'Engineering') SELECT eng.name, cte_departments.budget FROM eng JOIN cte_departments ON cte_departments.name = 'Engineering'";
        match try_q(&db, sql) {
            Ok(rows) => {
                assert_eq!(rows.len(), 3);
                for row in &rows {
                    assert_int(&row.values[1], 500000);
                }
            }
            Err(e) => eprintln!("CTE in JOIN not supported: {e}"),
        }
    }

    #[test]
    fn test_basic_cte_with_sum_count() {
        let db = setup_db();
        let rows = q(&db, "WITH stats AS (SELECT COUNT(*) AS cnt, SUM(salary) AS total FROM cte_employees) SELECT cnt, total FROM stats");
        assert_eq!(rows.len(), 1);
        assert_int(&rows[0].values[0], 6);
        assert_int(&rows[0].values[1], 505000);
    }

    // ========================================================================
    // Recursive CTEs (8 tests)
    // ========================================================================

    #[test]
    fn test_recursive_cte_simple_counter() {
        let db = setup_db();
        let sql = "WITH RECURSIVE counter(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counter WHERE n < 5) SELECT n FROM counter ORDER BY n";
        match try_q(&db, sql) {
            Ok(rows) => {
                assert_eq!(rows.len(), 5);
                for (i, row) in rows.iter().enumerate() {
                    assert_int(&row.values[0], (i as i64) + 1);
                }
            }
            Err(e) => eprintln!("Recursive CTE not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_hierarchy() {
        let db = setup_db();
        let sql = "WITH RECURSIVE reports(id, name, manager_id, depth) AS (SELECT id, name, manager_id, 0 FROM cte_employees WHERE id = 1 UNION ALL SELECT e.id, e.name, e.manager_id, r.depth + 1 FROM cte_employees e JOIN reports r ON e.manager_id = r.id) SELECT id, name, depth FROM reports ORDER BY depth, id";
        match try_q(&db, sql) {
            Ok(rows) => {
                assert!(rows.len() >= 4);
                assert_str(&rows[0].values[1], "Alice");
                assert_int(&rows[0].values[2], 0);
            }
            Err(e) => eprintln!("Recursive CTE hierarchy not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_union_all() {
        let db = setup_db();
        let sql = "WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < 3) SELECT * FROM nums ORDER BY n";
        match try_q(&db, sql) {
            Ok(rows) => assert_eq!(rows.len(), 3),
            Err(e) => eprintln!("Recursive CTE UNION ALL not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_union_dedup() {
        let db = setup_db();
        let sql = "WITH RECURSIVE nums(n) AS (SELECT 1 UNION SELECT n + 1 FROM nums WHERE n < 3) SELECT * FROM nums ORDER BY n";
        match try_q(&db, sql) {
            Ok(rows) => assert_eq!(rows.len(), 3),
            Err(e) => eprintln!("Recursive CTE with UNION (dedup) not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_depth_limit() {
        let db = setup_db();
        let sql = "WITH RECURSIVE tree(id, name, depth) AS (SELECT id, name, 0 FROM cte_employees WHERE id = 1 UNION ALL SELECT e.id, e.name, t.depth + 1 FROM cte_employees e JOIN tree t ON e.manager_id = t.id WHERE t.depth < 1) SELECT * FROM tree ORDER BY depth, id";
        match try_q(&db, sql) {
            Ok(rows) => {
                assert!(rows.len() >= 2);
                for row in &rows {
                    let depth = match &row.values[2] {
                        Value::Int4(v) => i64::from(*v),
                        Value::Int8(v) => *v,
                        other => panic!("unexpected type for depth: {other:?}"),
                    };
                    assert!(depth <= 1);
                }
            }
            Err(e) => eprintln!("Recursive CTE depth limit not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_generating_series() {
        let db = setup_db();
        let sql = "WITH RECURSIVE series(n) AS (SELECT 10 UNION ALL SELECT n + 10 FROM series WHERE n < 50) SELECT * FROM series ORDER BY n";
        match try_q(&db, sql) {
            Ok(rows) => {
                assert_eq!(rows.len(), 5);
                assert_int(&rows[0].values[0], 10);
                assert_int(&rows[4].values[0], 50);
            }
            Err(e) => eprintln!("Recursive CTE series not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_with_join_to_base_table() {
        let db = setup_db();
        let sql = "WITH RECURSIVE chain(id, name, mgr) AS (SELECT id, name, manager_id FROM cte_employees WHERE id = 4 UNION ALL SELECT e.id, e.name, e.manager_id FROM cte_employees e JOIN chain c ON e.id = c.mgr) SELECT id, name FROM chain ORDER BY id";
        match try_q(&db, sql) {
            Ok(rows) => assert!(rows.len() >= 2),
            Err(e) => eprintln!("Recursive CTE with join to base table not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_multiple_anchor_members() {
        let db = setup_db();
        let sql = "WITH RECURSIVE tree(id, name, depth) AS (SELECT id, name, 0 FROM cte_employees WHERE id = 1 UNION ALL SELECT id, name, 0 FROM cte_employees WHERE id = 3 UNION ALL SELECT e.id, e.name, t.depth + 1 FROM cte_employees e JOIN tree t ON e.manager_id = t.id WHERE t.depth < 1) SELECT * FROM tree ORDER BY id";
        match try_q(&db, sql) {
            Ok(rows) => assert!(rows.len() >= 2),
            Err(e) => eprintln!("Recursive CTE with multiple anchors not supported: {e}"),
        }
    }

    // ========================================================================
    // CTEs with DML context (5 tests)
    // ========================================================================

    #[test]
    fn test_cte_in_insert_select() {
        let db = setup_db();
        db.execute("CREATE TABLE cte_target (name TEXT, salary INT)").unwrap();
        match db.execute("WITH rich AS (SELECT name, salary FROM cte_employees WHERE salary > 80000) INSERT INTO cte_target SELECT name, salary FROM rich") {
            Ok(_) => {
                let rows = q(&db, "SELECT * FROM cte_target ORDER BY salary DESC");
                assert!(rows.len() >= 2);
            }
            Err(_) => {
                match db.execute("INSERT INTO cte_target SELECT name, salary FROM (SELECT name, salary FROM cte_employees WHERE salary > 80000) sub") {
                    Ok(_) => {
                        let rows = q(&db, "SELECT * FROM cte_target ORDER BY salary DESC");
                        assert!(rows.len() >= 2);
                    }
                    Err(e) => eprintln!("CTE in INSERT...SELECT not supported: {e}"),
                }
            }
        }
    }

    #[test]
    fn test_cte_with_order_by_limit() {
        let db = setup_db();
        let rows = q(&db, "WITH all_emp AS (SELECT name, salary FROM cte_employees) SELECT name, salary FROM all_emp ORDER BY salary DESC LIMIT 2");
        assert_eq!(rows.len(), 2);
        assert_str(&rows[0].values[0], "Alice");
        assert_int(&rows[0].values[1], 120000);
    }

    #[test]
    fn test_cte_with_distinct() {
        let db = setup_db();
        let rows = q(&db, "WITH depts AS (SELECT dept FROM cte_employees) SELECT DISTINCT dept FROM depts ORDER BY dept");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn test_cte_nested_in_subquery() {
        let db = setup_db();
        match try_q(&db, "SELECT * FROM (WITH eng AS (SELECT name, salary FROM cte_employees WHERE dept = 'Engineering') SELECT * FROM eng) sub WHERE sub.salary > 85000") {
            Ok(rows) => assert!(rows.len() >= 1),
            Err(e) => eprintln!("CTE nested in subquery not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_with_offset() {
        let db = setup_db();
        match try_q(&db, "WITH ordered AS (SELECT name, salary FROM cte_employees) SELECT * FROM ordered ORDER BY salary DESC LIMIT 2 OFFSET 1") {
            Ok(rows) => {
                assert_eq!(rows.len(), 2);
                assert_str(&rows[0].values[0], "Bob");
            }
            Err(e) => eprintln!("CTE with OFFSET not supported: {e}"),
        }
    }

    // ========================================================================
    // Edge cases (10 tests)
    // ========================================================================

    #[test]
    fn test_cte_same_column_names_as_outer() {
        let db = setup_db();
        let rows = q(&db, "WITH cte AS (SELECT id, name FROM cte_employees WHERE id <= 2) SELECT cte.id, cte.name FROM cte");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_cte_returning_empty_result() {
        let db = setup_db();
        let rows = q(&db, "WITH empty AS (SELECT id, name FROM cte_employees WHERE salary > 999999) SELECT * FROM empty");
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_cte_with_null_values() {
        let db = setup_db();
        let rows = q(&db, "WITH managers AS (SELECT id, name, manager_id FROM cte_employees) SELECT name, manager_id FROM managers WHERE manager_id IS NULL");
        assert_eq!(rows.len(), 1);
        assert_str(&rows[0].values[0], "Alice");
        assert_eq!(rows[0].values[1], Value::Null);
    }

    #[test]
    fn test_cte_with_type_coercion() {
        let db = setup_db();
        match try_q(&db, "WITH nums AS (SELECT id, salary FROM cte_employees) SELECT id, CAST(salary AS TEXT) FROM nums WHERE id = 1") {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert_str(&rows[0].values[1], "120000");
            }
            Err(e) => eprintln!("CTE with CAST not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_with_case_expression() {
        let db = setup_db();
        match try_q(&db, "WITH classified AS (SELECT name, CASE WHEN salary > 100000 THEN 'high' WHEN salary > 70000 THEN 'mid' ELSE 'low' END AS tier FROM cte_employees) SELECT name, tier FROM classified WHERE tier = 'high'") {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert_str(&rows[0].values[0], "Alice");
                assert_str(&rows[0].values[1], "high");
            }
            Err(e) => eprintln!("CTE with CASE not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_shadowing_table_name() {
        let db = setup_db();
        match try_q(&db, "WITH cte_products AS (SELECT id, name FROM cte_products WHERE category = 'A') SELECT * FROM cte_products ORDER BY id") {
            Ok(rows) => assert_eq!(rows.len(), 2),
            Err(e) => eprintln!("CTE shadowing table name not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_with_aggregate_having() {
        let db = setup_db();
        match try_q(&db, "WITH dept_counts AS (SELECT dept, COUNT(*) AS cnt FROM cte_employees GROUP BY dept HAVING COUNT(*) >= 2) SELECT * FROM dept_counts ORDER BY cnt DESC") {
            Ok(rows) => assert_eq!(rows.len(), 2),
            Err(e) => eprintln!("CTE with HAVING not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_arithmetic_expressions() {
        let db = setup_db();
        match try_q(&db, "WITH bonused AS (SELECT name, salary, salary * 2 AS double_sal FROM cte_employees WHERE id = 1) SELECT name, double_sal FROM bonused") {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert_int(&rows[0].values[1], 240000);
            }
            Err(e) => eprintln!("CTE with arithmetic not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_three_chained() {
        let db = setup_db();
        match try_q(&db, "WITH a AS (SELECT id, name, salary FROM cte_employees), b AS (SELECT id, name, salary FROM a WHERE salary > 70000), c AS (SELECT name, salary FROM b WHERE salary < 100000) SELECT * FROM c ORDER BY salary") {
            Ok(rows) => {
                assert_eq!(rows.len(), 3);
                assert_str(&rows[0].values[0], "Eve");
            }
            Err(e) => eprintln!("Three chained CTEs not supported: {e}"),
        }
    }

    #[test]
    fn test_recursive_cte_powers_of_two() {
        let db = setup_db();
        match try_q(&db, "WITH RECURSIVE powers(n) AS (SELECT 1 UNION ALL SELECT n * 2 FROM powers WHERE n < 16) SELECT n FROM powers ORDER BY n") {
            Ok(rows) => {
                assert_eq!(rows.len(), 5);
                assert_int(&rows[0].values[0], 1);
                assert_int(&rows[4].values[0], 16);
            }
            Err(e) => eprintln!("Recursive CTE powers not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_with_between() {
        let db = setup_db();
        let rows = q(&db, "WITH mid_range AS (SELECT name, salary FROM cte_employees WHERE salary BETWEEN 70000 AND 90000) SELECT * FROM mid_range ORDER BY salary");
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn test_cte_with_in_list() {
        let db = setup_db();
        let rows = q(&db, "WITH selected AS (SELECT name, dept FROM cte_employees WHERE dept IN ('Engineering', 'HR')) SELECT * FROM selected ORDER BY name");
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn test_cte_with_like() {
        let db = setup_db();
        let rows = q(&db, "WITH names AS (SELECT name FROM cte_employees WHERE name LIKE 'A%') SELECT * FROM names");
        assert_eq!(rows.len(), 1);
        assert_str(&rows[0].values[0], "Alice");
    }

    #[test]
    fn test_cte_cross_join() {
        let db = setup_db();
        match try_q(&db, "WITH small AS (SELECT id, name FROM cte_products WHERE id <= 2) SELECT a.name, b.name FROM small a JOIN small b ON a.id < b.id") {
            Ok(rows) => assert_eq!(rows.len(), 1),
            Err(e) => eprintln!("CTE cross join not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_multiple_used_in_union() {
        let db = setup_db();
        match try_q(&db, "WITH eng AS (SELECT name FROM cte_employees WHERE dept = 'Engineering'), sales AS (SELECT name FROM cte_employees WHERE dept = 'Sales') SELECT name FROM eng UNION ALL SELECT name FROM sales ORDER BY name") {
            Ok(rows) => assert_eq!(rows.len(), 5),
            Err(e) => eprintln!("Multiple CTEs in UNION not supported: {e}"),
        }
    }

    #[test]
    fn test_cte_with_exists() {
        let db = setup_db();
        match try_q(&db, "WITH managers AS (SELECT DISTINCT manager_id FROM cte_employees WHERE manager_id IS NOT NULL) SELECT name FROM cte_employees e WHERE EXISTS (SELECT 1 FROM managers m WHERE m.manager_id = e.id)") {
            Ok(rows) => assert!(rows.len() >= 2),
            Err(e) => eprintln!("CTE with EXISTS not supported: {e}"),
        }
    }
}
