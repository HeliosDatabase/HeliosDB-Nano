//! Tests for named window references (WINDOW clause) in HeliosDB Nano.
//!
//! Covers: basic WINDOW clause, multiple named windows, mixed inline + named,
//! WINDOW with only ORDER BY, error on undefined window name, and window
//! inheritance (w2 extending w1).

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod named_window_tests {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    // ========================================================================
    // Helpers
    // ========================================================================

    fn db() -> EmbeddedDatabase {
        EmbeddedDatabase::new_in_memory().expect("Failed to create in-memory database")
    }

    fn to_i64(v: &Value) -> i64 {
        match v {
            Value::Int2(n) => *n as i64,
            Value::Int4(n) => *n as i64,
            Value::Int8(n) => *n,
            Value::Float4(f) => *f as i64,
            Value::Float8(f) => *f as i64,
            Value::Numeric(s) => s.parse::<f64>().unwrap() as i64,
            other => panic!("Expected numeric value, got {:?}", other),
        }
    }

    fn to_string(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            other => panic!("Expected string value, got {:?}", other),
        }
    }

    fn setup_employees(d: &EmbeddedDatabase) {
        d.execute("CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, dept TEXT, salary INT)")
            .unwrap();
        d.execute("INSERT INTO employees VALUES (1, 'Alice',   'Engineering', 120000)").unwrap();
        d.execute("INSERT INTO employees VALUES (2, 'Bob',     'Engineering', 110000)").unwrap();
        d.execute("INSERT INTO employees VALUES (3, 'Charlie', 'Engineering', 105000)").unwrap();
        d.execute("INSERT INTO employees VALUES (4, 'Dave',    'Sales',        90000)").unwrap();
        d.execute("INSERT INTO employees VALUES (5, 'Eve',     'Sales',        95000)").unwrap();
        d.execute("INSERT INTO employees VALUES (6, 'Frank',   'Marketing',    80000)").unwrap();
    }

    // ========================================================================
    // Test 1: Basic WINDOW clause with one named window used by multiple fns
    // ========================================================================

    #[test]
    fn test_basic_named_window_multiple_functions() {
        let d = db();
        setup_employees(&d);

        // Use WINDOW w to define the spec once, reference it from two functions
        let rows = d.query(
            "SELECT name, dept, \
                ROW_NUMBER() OVER w, \
                SUM(salary) OVER w \
             FROM employees \
             WINDOW w AS (PARTITION BY dept ORDER BY salary)",
            &[],
        ).unwrap();

        assert_eq!(rows.len(), 6);

        // Collect results per department to verify
        let mut dept_data: std::collections::HashMap<String, Vec<(String, i64, i64)>> =
            std::collections::HashMap::new();
        for row in &rows {
            let name = to_string(row.get(0).unwrap());
            let dept = to_string(row.get(1).unwrap());
            let rn = to_i64(row.get(2).unwrap());
            let running_sum = to_i64(row.get(3).unwrap());
            dept_data.entry(dept).or_default().push((name, rn, running_sum));
        }

        // Engineering: 3 employees, ordered by salary (105000, 110000, 120000)
        let eng = dept_data.get("Engineering").unwrap();
        let mut eng_rns: Vec<i64> = eng.iter().map(|x| x.1).collect();
        eng_rns.sort();
        assert_eq!(eng_rns, vec![1, 2, 3], "Engineering should have ROW_NUMBERs 1,2,3");

        // Sales: 2 employees, ordered by salary (90000, 95000)
        let sales = dept_data.get("Sales").unwrap();
        let mut sales_rns: Vec<i64> = sales.iter().map(|x| x.1).collect();
        sales_rns.sort();
        assert_eq!(sales_rns, vec![1, 2], "Sales should have ROW_NUMBERs 1,2");
    }

    // ========================================================================
    // Test 2: WINDOW with PARTITION BY and ORDER BY
    // ========================================================================

    #[test]
    fn test_named_window_partition_and_order() {
        let d = db();
        setup_employees(&d);

        let rows = d.query(
            "SELECT name, dept, RANK() OVER w \
             FROM employees \
             WINDOW w AS (PARTITION BY dept ORDER BY salary DESC)",
            &[],
        ).unwrap();

        assert_eq!(rows.len(), 6);

        // Within Engineering, highest salary (Alice=120000) should be rank 1
        for row in &rows {
            let name = to_string(row.get(0).unwrap());
            let rank = to_i64(row.get(2).unwrap());
            if name == "Alice" {
                assert_eq!(rank, 1, "Alice should be rank 1 in Engineering (highest salary)");
            }
        }

        // Within Marketing, only Frank, so rank 1
        for row in &rows {
            let name = to_string(row.get(0).unwrap());
            let rank = to_i64(row.get(2).unwrap());
            if name == "Frank" {
                assert_eq!(rank, 1, "Frank should be rank 1 in Marketing (only employee)");
            }
        }
    }

    // ========================================================================
    // Test 3: Multiple named windows in same query
    // ========================================================================

    #[test]
    fn test_multiple_named_windows() {
        let d = db();
        setup_employees(&d);

        // Two named windows: w1 and w2 with same partition and order,
        // used by different window functions to verify both resolve correctly
        let rows = d.query(
            "SELECT name, dept, salary, \
                ROW_NUMBER() OVER w1, \
                SUM(salary) OVER w2 \
             FROM employees \
             WINDOW w1 AS (PARTITION BY dept ORDER BY salary), \
                    w2 AS (PARTITION BY dept ORDER BY salary)",
            &[],
        ).unwrap();

        assert_eq!(rows.len(), 6);

        // Verify both window functions resolved independently from their named defs
        // Engineering: ordered by salary (105000, 110000, 120000)
        // ROW_NUMBER: 1, 2, 3
        // Running SUM: 105000, 215000, 335000
        for row in &rows {
            let name = to_string(row.get(0).unwrap());
            let rn = to_i64(row.get(3).unwrap());
            let running_sum = to_i64(row.get(4).unwrap());
            if name == "Charlie" {
                // Charlie has lowest Engineering salary (105000) -> rn=1
                assert_eq!(rn, 1, "Charlie should be ROW_NUMBER 1 (lowest salary in Engineering)");
                assert_eq!(running_sum, 105000, "Charlie running sum should be 105000");
            }
            if name == "Frank" {
                // Only Marketing employee -> rn=1
                assert_eq!(rn, 1, "Frank should be ROW_NUMBER 1 (only Marketing employee)");
                assert_eq!(running_sum, 80000, "Frank running sum should be 80000");
            }
        }
    }

    // ========================================================================
    // Test 4: Mix of inline and named window in same query
    // ========================================================================

    #[test]
    fn test_mixed_inline_and_named_window() {
        let d = db();
        setup_employees(&d);

        // w1 is a named window; the second function uses an inline OVER clause
        // Both use the same partitioning to avoid multi-partition executor issues
        let rows = d.query(
            "SELECT name, dept, salary, \
                ROW_NUMBER() OVER w1, \
                RANK() OVER (PARTITION BY dept ORDER BY salary DESC) \
             FROM employees \
             WINDOW w1 AS (PARTITION BY dept ORDER BY salary ASC)",
            &[],
        ).unwrap();

        assert_eq!(rows.len(), 6);

        // For each employee, w1 (ASC) + inline (DESC) rank should sum to N+1
        for row in &rows {
            let dept = to_string(row.get(1).unwrap());
            let rn_named = to_i64(row.get(3).unwrap());
            let rn_inline = to_i64(row.get(4).unwrap());
            if dept == "Engineering" {
                // 3 employees: rn_asc + rn_desc = 4
                assert_eq!(
                    rn_named + rn_inline, 4,
                    "Engineering: named(asc) + inline(desc) rank should sum to 4"
                );
            } else if dept == "Marketing" {
                assert_eq!(rn_named, 1);
                assert_eq!(rn_inline, 1);
            }
        }
    }

    // ========================================================================
    // Test 5: Error on undefined window name
    // ========================================================================

    #[test]
    fn test_undefined_window_name_error() {
        let d = db();
        setup_employees(&d);

        // Reference a window name that was never defined
        let result = d.query(
            "SELECT name, ROW_NUMBER() OVER nonexistent \
             FROM employees",
            &[],
        );

        assert!(result.is_err(), "Should error when referencing undefined window name");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("nonexistent") || err_msg.contains("not defined"),
            "Error should mention the undefined window name, got: {}",
            err_msg
        );
    }

    // ========================================================================
    // Test 6: WINDOW with only ORDER BY (no PARTITION BY)
    // ========================================================================

    #[test]
    fn test_named_window_order_by_only() {
        let d = db();
        setup_employees(&d);

        let rows = d.query(
            "SELECT name, salary, ROW_NUMBER() OVER w \
             FROM employees \
             WINDOW w AS (ORDER BY salary DESC)",
            &[],
        ).unwrap();

        assert_eq!(rows.len(), 6);

        // Ordered by salary DESC: 120000, 110000, 105000, 95000, 90000, 80000
        // Find the row with rn=1 - should have highest salary
        for row in &rows {
            let salary = to_i64(row.get(1).unwrap());
            let rn = to_i64(row.get(2).unwrap());
            if rn == 1 {
                assert_eq!(salary, 120000, "ROW_NUMBER=1 should have highest salary (120000)");
            }
            if rn == 6 {
                assert_eq!(salary, 80000, "ROW_NUMBER=6 should have lowest salary (80000)");
            }
        }
    }

    // ========================================================================
    // Test 7: Window inheritance (w2 extends w1)
    // ========================================================================

    #[test]
    fn test_window_inheritance() {
        let d = db();
        setup_employees(&d);

        // w1 defines PARTITION BY, w2 inherits partition and adds ORDER BY
        let rows = d.query(
            "SELECT name, dept, salary, ROW_NUMBER() OVER w2 \
             FROM employees \
             WINDOW w1 AS (PARTITION BY dept), \
                    w2 AS (w1 ORDER BY salary)",
            &[],
        ).unwrap();

        assert_eq!(rows.len(), 6);

        // w2 should effectively be (PARTITION BY dept ORDER BY salary)
        let mut dept_data: std::collections::HashMap<String, Vec<(i64, i64)>> =
            std::collections::HashMap::new();
        for row in &rows {
            let dept = to_string(row.get(1).unwrap());
            let salary = to_i64(row.get(2).unwrap());
            let rn = to_i64(row.get(3).unwrap());
            dept_data.entry(dept).or_default().push((salary, rn));
        }

        // Engineering: 3 employees ordered by salary -> rn 1,2,3
        let mut eng = dept_data.get("Engineering").unwrap().clone();
        eng.sort_by_key(|x| x.0);
        assert_eq!(eng.len(), 3);
        assert_eq!(eng[0].1, 1, "Lowest salary in Engineering should be ROW_NUMBER 1");
        assert_eq!(eng[1].1, 2, "Middle salary in Engineering should be ROW_NUMBER 2");
        assert_eq!(eng[2].1, 3, "Highest salary in Engineering should be ROW_NUMBER 3");

        // Sales: 2 employees
        let mut sales = dept_data.get("Sales").unwrap().clone();
        sales.sort_by_key(|x| x.0);
        assert_eq!(sales.len(), 2);
        assert_eq!(sales[0].1, 1, "Lower salary in Sales should be ROW_NUMBER 1");
        assert_eq!(sales[1].1, 2, "Higher salary in Sales should be ROW_NUMBER 2");
    }

    // ========================================================================
    // Test 8: Named window with aggregate window function (SUM OVER w)
    // ========================================================================

    #[test]
    fn test_named_window_aggregate_function() {
        let d = db();
        d.execute("CREATE TABLE scores (id INT PRIMARY KEY, team TEXT, score INT)").unwrap();
        d.execute("INSERT INTO scores VALUES (1, 'A', 10)").unwrap();
        d.execute("INSERT INTO scores VALUES (2, 'A', 20)").unwrap();
        d.execute("INSERT INTO scores VALUES (3, 'A', 30)").unwrap();
        d.execute("INSERT INTO scores VALUES (4, 'B', 5)").unwrap();
        d.execute("INSERT INTO scores VALUES (5, 'B', 15)").unwrap();

        let rows = d.query(
            "SELECT team, score, \
                SUM(score) OVER w, \
                COUNT(score) OVER w \
             FROM scores \
             WINDOW w AS (PARTITION BY team ORDER BY score)",
            &[],
        ).unwrap();

        assert_eq!(rows.len(), 5);

        // For team A ordered by score (10, 20, 30):
        // Running SUM: 10, 30, 60
        // Running COUNT: 1, 2, 3
        for row in &rows {
            let team = to_string(row.get(0).unwrap());
            let score = to_i64(row.get(1).unwrap());
            let running_sum = to_i64(row.get(2).unwrap());
            let running_count = to_i64(row.get(3).unwrap());

            if team == "A" && score == 10 {
                assert_eq!(running_sum, 10, "Team A, score=10: running sum should be 10");
                assert_eq!(running_count, 1, "Team A, score=10: running count should be 1");
            }
            if team == "A" && score == 30 {
                assert_eq!(running_sum, 60, "Team A, score=30: running sum should be 60");
                assert_eq!(running_count, 3, "Team A, score=30: running count should be 3");
            }
            if team == "B" && score == 5 {
                assert_eq!(running_sum, 5, "Team B, score=5: running sum should be 5");
                assert_eq!(running_count, 1, "Team B, score=5: running count should be 1");
            }
        }
    }
}
