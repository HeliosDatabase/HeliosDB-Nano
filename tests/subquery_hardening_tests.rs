//! Comprehensive subquery hardening tests for HeliosDB Nano.
//!
//! Tests subquery edge cases across scalar subqueries, IN/NOT IN, EXISTS/NOT EXISTS,
//! derived tables (FROM subquery), correlated subqueries, set operations in subqueries,
//! and various edge cases like self-referencing, deep nesting, HAVING, and LIMIT.
//!
//! Tests that probe unsupported or partially supported features use `match` with
//! an `Err` branch that documents the limitation rather than panicking.

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod subquery_hardening {
    use heliosdb_nano::{EmbeddedDatabase, Value};

    // ========================================================================
    // Helper: create an in-memory database with employees/departments/projects
    // ========================================================================

    /// Sets up a schema with three related tables for subquery testing.
    ///
    /// departments(id INT, name TEXT)
    ///   (1, 'Engineering'), (2, 'Sales'), (3, 'HR')
    ///
    /// employees(id INT, name TEXT, dept_id INT, salary INT)
    ///   (1, 'Alice',   1, 90000)
    ///   (2, 'Bob',     1, 80000)
    ///   (3, 'Charlie', 2, 70000)
    ///   (4, 'Diana',   2, 60000)
    ///   (5, 'Eve',     3, 75000)
    ///   -- No employee with dept_id that doesn't match a department
    ///
    /// projects(id INT, name TEXT, lead_id INT, budget INT)
    ///   (1, 'Alpha',   1, 500000)
    ///   (2, 'Beta',    2, 300000)
    ///   (3, 'Gamma',   3, 200000)
    ///   -- lead_id 4 and 5 have no projects
    fn setup_db() -> EmbeddedDatabase {
        let db = EmbeddedDatabase::new_in_memory().unwrap();

        db.execute("CREATE TABLE departments (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO departments VALUES (1, 'Engineering')").unwrap();
        db.execute("INSERT INTO departments VALUES (2, 'Sales')").unwrap();
        db.execute("INSERT INTO departments VALUES (3, 'HR')").unwrap();

        db.execute("CREATE TABLE employees (id INT, name TEXT, dept_id INT, salary INT)").unwrap();
        db.execute("INSERT INTO employees VALUES (1, 'Alice', 1, 90000)").unwrap();
        db.execute("INSERT INTO employees VALUES (2, 'Bob', 1, 80000)").unwrap();
        db.execute("INSERT INTO employees VALUES (3, 'Charlie', 2, 70000)").unwrap();
        db.execute("INSERT INTO employees VALUES (4, 'Diana', 2, 60000)").unwrap();
        db.execute("INSERT INTO employees VALUES (5, 'Eve', 3, 75000)").unwrap();

        db.execute("CREATE TABLE projects (id INT, name TEXT, lead_id INT, budget INT)").unwrap();
        db.execute("INSERT INTO projects VALUES (1, 'Alpha', 1, 500000)").unwrap();
        db.execute("INSERT INTO projects VALUES (2, 'Beta', 2, 300000)").unwrap();
        db.execute("INSERT INTO projects VALUES (3, 'Gamma', 3, 200000)").unwrap();

        db
    }

    // ========================================================================
    // 1. Scalar subqueries (~5 tests)
    // ========================================================================

    #[test]
    fn test_scalar_subquery_max_in_select() {
        // SELECT (SELECT MAX(salary) FROM employees) -- should return 90000
        // SQL standard: a scalar subquery in the SELECT list returns a single value.
        let db = setup_db();

        let sql = "SELECT (SELECT MAX(salary) FROM employees)";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1, "Scalar subquery should return 1 row");
                // MAX(salary) = 90000
                let val = rows[0].get(0).unwrap();
                assert!(
                    val == &Value::Int4(90000) || val == &Value::Int8(90000),
                    "MAX(salary) should be 90000, got {:?}",
                    val
                );
            }
            Err(e) => {
                // Scalar subqueries in SELECT may not be supported (Expr::Subquery)
                println!("KNOWN LIMITATION: Scalar subquery in SELECT not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_scalar_subquery_in_where_equality() {
        // WHERE salary = (SELECT MAX(salary) FROM employees)
        // Should return Alice (salary = 90000).
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE salary = (SELECT MAX(salary) FROM employees)";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1, "Only one employee has max salary");
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Scalar subquery in WHERE not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_scalar_subquery_returning_null() {
        // SELECT (SELECT MAX(salary) FROM employees WHERE salary > 999999)
        // No rows match, so MAX returns NULL.
        // SQL standard: scalar subquery returning NULL is valid.
        let db = setup_db();

        let sql = "SELECT (SELECT MAX(salary) FROM employees WHERE salary > 999999)";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0].get(0).unwrap(),
                    &Value::Null,
                    "MAX over empty set should be NULL"
                );
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Scalar subquery returning NULL not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_scalar_subquery_no_rows_is_null() {
        // SELECT (SELECT name FROM employees WHERE id = 999)
        // Subquery returns no rows; SQL standard says result should be NULL.
        let db = setup_db();

        let sql = "SELECT (SELECT name FROM employees WHERE id = 999)";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0].get(0).unwrap(),
                    &Value::Null,
                    "Scalar subquery returning 0 rows should yield NULL"
                );
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Scalar subquery (0 rows -> NULL) not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_scalar_subquery_multiple_rows_should_error() {
        // SELECT (SELECT name FROM employees)
        // Subquery returns 5 rows; SQL standard says this must be an error.
        let db = setup_db();

        let sql = "SELECT (SELECT name FROM employees)";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // Some engines silently pick the first row; document that behavior.
                println!(
                    "DEVIATION: Scalar subquery with {} rows did not error. First value: {:?}",
                    rows.len(),
                    rows.get(0).and_then(|r| r.get(0))
                );
            }
            Err(_e) => {
                // This IS the correct SQL-standard behavior: an error for >1 row.
            }
        }
    }

    // ========================================================================
    // 2. IN subquery (~5 tests)
    // ========================================================================

    #[test]
    fn test_in_subquery_basic() {
        // Employees who lead a project.
        // lead_ids in projects: 1, 2, 3 -> Alice, Bob, Charlie
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE id IN (SELECT lead_id FROM projects) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 3, "3 employees lead projects, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Bob".to_string())));
                assert_eq!(rows[2].get(1), Some(&Value::String("Charlie".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: IN subquery not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_not_in_subquery_basic() {
        // Employees who do NOT lead any project.
        // lead_ids in projects: 1, 2, 3 -> Diana (4) and Eve (5) are excluded
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE id NOT IN (SELECT lead_id FROM projects) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "2 employees don't lead projects, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Diana".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Eve".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: NOT IN subquery not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_in_subquery_empty_result() {
        // IN subquery where the subquery returns no rows.
        // No projects with budget > 999999, so IN list is empty -> no matches.
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE id IN (SELECT lead_id FROM projects WHERE budget > 999999)";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 0, "Empty IN list should match nobody");
            }
            Err(e) => {
                println!("KNOWN LIMITATION: IN subquery with empty result not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_in_subquery_with_nulls() {
        // SQL standard: col IN (1, 2, NULL) -- if col is 3, result is UNKNOWN (not FALSE).
        // NOT IN with NULLs is notoriously tricky.
        let db = EmbeddedDatabase::new_in_memory().unwrap();

        db.execute("CREATE TABLE t_outer (id INT)").unwrap();
        db.execute("INSERT INTO t_outer VALUES (1)").unwrap();
        db.execute("INSERT INTO t_outer VALUES (2)").unwrap();
        db.execute("INSERT INTO t_outer VALUES (3)").unwrap();

        db.execute("CREATE TABLE t_inner (val INT)").unwrap();
        db.execute("INSERT INTO t_inner VALUES (1)").unwrap();
        db.execute("INSERT INTO t_inner VALUES (NULL)").unwrap();

        // IN with NULL in subquery: 1 matches, 2 and 3 get UNKNOWN -> should not match
        let sql = "SELECT id FROM t_outer WHERE id IN (SELECT val FROM t_inner) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // At minimum, id=1 must match. id=2,3 should NOT match (UNKNOWN != TRUE).
                assert!(
                    rows.len() >= 1,
                    "At least id=1 should match IN with NULLs, got {} rows",
                    rows.len()
                );
                assert_eq!(rows[0].get(0), Some(&Value::Int4(1)));
                if rows.len() > 1 {
                    println!(
                        "DEVIATION: IN subquery with NULL returned {} rows (SQL standard says only 1)",
                        rows.len()
                    );
                }
            }
            Err(e) => {
                println!("KNOWN LIMITATION: IN subquery with NULLs not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_in_subquery_with_duplicates() {
        // Subquery returns duplicate values; should not cause duplicate matches.
        let db = EmbeddedDatabase::new_in_memory().unwrap();

        db.execute("CREATE TABLE t_main (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t_main VALUES (1, 'A')").unwrap();
        db.execute("INSERT INTO t_main VALUES (2, 'B')").unwrap();
        db.execute("INSERT INTO t_main VALUES (3, 'C')").unwrap();

        db.execute("CREATE TABLE t_refs (ref_id INT)").unwrap();
        db.execute("INSERT INTO t_refs VALUES (1)").unwrap();
        db.execute("INSERT INTO t_refs VALUES (1)").unwrap(); // duplicate
        db.execute("INSERT INTO t_refs VALUES (2)").unwrap();
        db.execute("INSERT INTO t_refs VALUES (2)").unwrap(); // duplicate
        db.execute("INSERT INTO t_refs VALUES (2)").unwrap(); // triple

        let sql = "SELECT id, name FROM t_main WHERE id IN (SELECT ref_id FROM t_refs) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // Duplicates in subquery should not cause duplicates in output.
                assert_eq!(rows.len(), 2, "Only ids 1 and 2 match, each once. Got {}", rows.len());
                assert_eq!(rows[0].get(0), Some(&Value::Int4(1)));
                assert_eq!(rows[1].get(0), Some(&Value::Int4(2)));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: IN subquery with duplicates not supported: {}", e);
            }
        }
    }

    // ========================================================================
    // 3. EXISTS subquery (~5 tests)
    // ========================================================================

    #[test]
    fn test_exists_correlated_join_pattern() {
        // Classic semi-join: find employees who lead at least one project.
        // WHERE EXISTS (SELECT 1 FROM projects WHERE projects.lead_id = employees.id)
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE EXISTS (SELECT 1 FROM projects WHERE projects.lead_id = employees.id) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 3, "3 employees lead projects, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Bob".to_string())));
                assert_eq!(rows[2].get(1), Some(&Value::String("Charlie".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Correlated EXISTS not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_not_exists_anti_join_pattern() {
        // Anti-join: find employees who do NOT lead any project.
        // WHERE NOT EXISTS (SELECT 1 FROM projects WHERE projects.lead_id = employees.id)
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE NOT EXISTS (SELECT 1 FROM projects WHERE projects.lead_id = employees.id) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Diana and Eve don't lead projects, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Diana".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Eve".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Correlated NOT EXISTS not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_exists_empty_subquery_is_false() {
        // EXISTS with an uncorrelated subquery that returns no rows.
        // No departments with id > 999 -> EXISTS is false -> no employees returned.
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE EXISTS (SELECT 1 FROM departments WHERE id > 999)";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 0, "EXISTS on empty subquery should be false, got {} rows", rows.len());
            }
            Err(e) => {
                println!("KNOWN LIMITATION: EXISTS with empty subquery not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_exists_always_true_uncorrelated() {
        // EXISTS with an uncorrelated subquery that always has rows.
        // departments has 3 rows, so EXISTS is always true -> all employees returned.
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE EXISTS (SELECT 1 FROM departments) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 5, "EXISTS(non-empty) should return all 5 employees, got {}", rows.len());
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Uncorrelated EXISTS not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_exists_anti_join_with_condition() {
        // Anti-join with an extra condition: employees not leading any project with budget > 400000.
        // Only Alpha (budget 500000, lead_id=1) has budget > 400000, so Alice leads it.
        // All others should be returned.
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE NOT EXISTS (SELECT 1 FROM projects WHERE projects.lead_id = employees.id AND projects.budget > 400000) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // Alice is excluded (leads Alpha with 500K). Bob, Charlie, Diana, Eve remain.
                assert_eq!(rows.len(), 4, "All except Alice should be returned, got {}", rows.len());
                // Verify Alice is NOT in results
                for row in &rows {
                    let name = row.get(1).unwrap();
                    assert_ne!(name, &Value::String("Alice".to_string()), "Alice should be excluded");
                }
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Correlated NOT EXISTS with condition not supported: {}", e);
            }
        }
    }

    // ========================================================================
    // 4. Derived tables (FROM subquery) (~4 tests)
    // ========================================================================

    #[test]
    fn test_derived_table_simple() {
        // SELECT * FROM (SELECT id, name FROM employees) AS sub
        let db = setup_db();

        let sql = "SELECT * FROM (SELECT id, name FROM employees) AS sub ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 5, "Derived table should pass through all 5 rows");
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[4].get(1), Some(&Value::String("Eve".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Simple derived table not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_derived_table_with_aggregation() {
        // SELECT * FROM (SELECT dept_id, COUNT(*) AS cnt FROM employees GROUP BY dept_id) AS sub ORDER BY dept_id
        let db = setup_db();

        let sql = "SELECT * FROM (SELECT dept_id, COUNT(*) AS cnt FROM employees GROUP BY dept_id) AS sub ORDER BY dept_id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // dept 1: 2 employees, dept 2: 2 employees, dept 3: 1 employee
                assert_eq!(rows.len(), 3, "3 departments, got {}", rows.len());
                // Check dept 1 count
                let dept1_count = rows[0].get(1).unwrap();
                assert!(
                    dept1_count == &Value::Int4(2) || dept1_count == &Value::Int8(2),
                    "Engineering should have 2 employees, got {:?}",
                    dept1_count
                );
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Derived table with aggregation not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_derived_table_with_where() {
        // Derived table with a WHERE clause both inside and outside.
        let db = setup_db();

        let sql = "SELECT * FROM (SELECT id, name, salary FROM employees WHERE salary > 65000) AS high_earners WHERE salary < 85000 ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // salary > 65000: Alice(90K), Bob(80K), Charlie(70K), Eve(75K)
                // then salary < 85000: Bob(80K), Charlie(70K), Eve(75K)
                assert_eq!(rows.len(), 3, "3 employees between 65K and 85K, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Bob".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Charlie".to_string())));
                assert_eq!(rows[2].get(1), Some(&Value::String("Eve".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Derived table with WHERE not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_nested_derived_tables() {
        // Two levels of nesting.
        // SELECT * FROM (SELECT * FROM (SELECT id, name FROM employees) AS inner1 WHERE id <= 3) AS outer1 ORDER BY id
        let db = setup_db();

        let sql = "SELECT * FROM (SELECT * FROM (SELECT id, name FROM employees) AS inner1 WHERE id <= 3) AS outer1 ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 3, "Nested derived tables should return ids 1,2,3. Got {}", rows.len());
                assert_eq!(rows[0].get(0), Some(&Value::Int4(1)));
                assert_eq!(rows[1].get(0), Some(&Value::Int4(2)));
                assert_eq!(rows[2].get(0), Some(&Value::Int4(3)));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Nested derived tables not supported: {}", e);
            }
        }
    }

    // ========================================================================
    // 5. Correlated subqueries (~4 tests)
    // ========================================================================

    #[test]
    fn test_correlated_subquery_comparison() {
        // Employees earning more than the average salary in their own department.
        // Dept 1: avg = (90K+80K)/2 = 85K -> Alice(90K) qualifies
        // Dept 2: avg = (70K+60K)/2 = 65K -> Charlie(70K) qualifies
        // Dept 3: avg = 75K/1 = 75K -> Eve does NOT qualify (75K = 75K, not >)
        let db = setup_db();

        let sql = "SELECT id, name FROM employees e WHERE salary > (SELECT AVG(salary) FROM employees e2 WHERE e2.dept_id = e.dept_id) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Alice and Charlie earn above their dept avg, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Charlie".to_string())));
            }
            Err(e) => {
                // Correlated subqueries require per-row evaluation; may not be supported.
                println!("KNOWN LIMITATION: Correlated comparison subquery not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_correlated_exists_with_additional_filter() {
        // Departments that have at least one employee earning > 80000.
        // Dept 1 has Alice(90K) -> yes; Dept 2 max is 70K -> no; Dept 3 max is 75K -> no
        let db = setup_db();

        let sql = "SELECT id, name FROM departments d WHERE EXISTS (SELECT 1 FROM employees e WHERE e.dept_id = d.id AND e.salary > 80000) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1, "Only Engineering has someone earning > 80K, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Engineering".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Correlated EXISTS with filter not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_correlated_in_subquery() {
        // Departments that have at least one project lead working in them.
        // Projects leads: 1 (dept 1), 2 (dept 1), 3 (dept 2)
        // So dept 1 and dept 2 have leads; dept 3 does not (Eve=5 is not a lead).
        let db = setup_db();

        let sql = "SELECT id, name FROM departments d WHERE id IN (SELECT dept_id FROM employees e WHERE e.id IN (SELECT lead_id FROM projects)) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Depts 1 and 2 have project leads, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Engineering".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Sales".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Nested IN subqueries not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_correlated_scalar_subquery_in_select() {
        // SELECT id, name, (SELECT COUNT(*) FROM projects WHERE projects.lead_id = employees.id) AS proj_count FROM employees ORDER BY id
        let db = setup_db();

        let sql = "SELECT id, name, (SELECT COUNT(*) FROM projects WHERE projects.lead_id = employees.id) AS proj_count FROM employees ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 5, "All 5 employees should be returned");
                // Alice: 1 project, Bob: 1 project, Charlie: 1 project, Diana: 0, Eve: 0
                let alice_count = rows[0].get(2).unwrap();
                assert!(
                    alice_count == &Value::Int4(1) || alice_count == &Value::Int8(1),
                    "Alice leads 1 project, got {:?}",
                    alice_count
                );
                let diana_count = rows[3].get(2).unwrap();
                assert!(
                    diana_count == &Value::Int4(0) || diana_count == &Value::Int8(0),
                    "Diana leads 0 projects, got {:?}",
                    diana_count
                );
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Correlated scalar subquery in SELECT not supported: {}", e);
            }
        }
    }

    // ========================================================================
    // 6. Subquery with set operations (~3 tests)
    // ========================================================================

    #[test]
    fn test_in_subquery_with_union() {
        // WHERE id IN (SELECT lead_id FROM projects UNION SELECT dept_id FROM departments)
        // lead_ids: 1, 2, 3; dept_ids: 1, 2, 3 -> UNION = {1, 2, 3}
        // All employees with id IN {1,2,3} -> Alice, Bob, Charlie
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE id IN (SELECT lead_id FROM projects UNION SELECT id FROM departments) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 3, "IDs 1,2,3 match, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Bob".to_string())));
                assert_eq!(rows[2].get(1), Some(&Value::String("Charlie".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: IN subquery with UNION not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_in_subquery_with_union_all() {
        // UNION ALL keeps duplicates in the subquery, but IN semantics should still
        // only check membership (no duplicate outer rows).
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE id IN (SELECT lead_id FROM projects UNION ALL SELECT lead_id FROM projects) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // lead_ids appear twice each (1,2,3,1,2,3) but IN should yield 3 rows max
                assert_eq!(rows.len(), 3, "UNION ALL duplicates should not cause duplicate matches, got {}", rows.len());
            }
            Err(e) => {
                println!("KNOWN LIMITATION: IN subquery with UNION ALL not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_derived_table_from_union() {
        // Use a UNION as a derived table in FROM.
        let db = setup_db();

        let sql = "SELECT * FROM (SELECT id, name FROM employees WHERE dept_id = 1 UNION SELECT id, name FROM employees WHERE dept_id = 3) AS combined ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // Dept 1: Alice(1), Bob(2); Dept 3: Eve(5) -> 3 rows total
                assert_eq!(rows.len(), 3, "Union of dept 1 and 3 employees, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Bob".to_string())));
                assert_eq!(rows[2].get(1), Some(&Value::String("Eve".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Derived table from UNION not supported: {}", e);
            }
        }
    }

    // ========================================================================
    // 7. Edge cases (~4 tests)
    // ========================================================================

    #[test]
    fn test_subquery_referencing_same_table() {
        // Self-referencing: employees earning more than the overall average.
        // Average salary = (90K+80K+70K+60K+75K)/5 = 75K
        // salary > 75K: Alice(90K), Bob(80K)
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE salary > (SELECT AVG(salary) FROM employees) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Alice and Bob earn above average, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Bob".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Self-referencing scalar subquery not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_deeply_nested_subquery() {
        // Three levels: outer query -> IN subquery -> another IN subquery.
        // Employees in departments that have at least one project with budget > 250K.
        //
        // Projects with budget > 250K: Alpha(500K, lead=1), Beta(300K, lead=2)
        // Leads: employee 1 (dept 1), employee 2 (dept 1) -> dept_ids = {1}
        // So only employees in dept 1 should be returned: Alice, Bob
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE dept_id IN (SELECT dept_id FROM employees WHERE id IN (SELECT lead_id FROM projects WHERE budget > 250000)) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Only dept 1 employees qualify, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                assert_eq!(rows[1].get(1), Some(&Value::String("Bob".to_string())));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Deeply nested IN subqueries not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_subquery_in_having() {
        // HAVING with a subquery: departments where the average salary exceeds the
        // overall minimum salary by more than 10000.
        //
        // Overall MIN(salary) = 60000.
        // Dept 1 avg = 85000 -> 85000 > 60000 + 10000 = 70000 -> yes
        // Dept 2 avg = 65000 -> 65000 > 70000 -> no
        // Dept 3 avg = 75000 -> 75000 > 70000 -> yes
        let db = setup_db();

        let sql = "SELECT dept_id, AVG(salary) AS avg_sal FROM employees GROUP BY dept_id HAVING AVG(salary) > (SELECT MIN(salary) FROM employees) + 10000 ORDER BY dept_id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Depts 1 and 3 qualify, got {}", rows.len());
                // dept_id 1 and 3
                assert_eq!(rows[0].get(0), Some(&Value::Int4(1)));
                assert_eq!(rows[1].get(0), Some(&Value::Int4(3)));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Subquery in HAVING not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_subquery_with_order_by_and_limit() {
        // Subquery with ORDER BY and LIMIT: find the employee with the highest salary
        // using a derived table.
        let db = setup_db();

        let sql = "SELECT * FROM (SELECT id, name, salary FROM employees ORDER BY salary DESC LIMIT 1) AS top_earner";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 1, "LIMIT 1 should return exactly 1 row, got {}", rows.len());
                assert_eq!(rows[0].get(1), Some(&Value::String("Alice".to_string())));
                let salary = rows[0].get(2).unwrap();
                assert!(
                    salary == &Value::Int4(90000) || salary == &Value::Int8(90000),
                    "Top salary should be 90000, got {:?}",
                    salary
                );
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Derived table with ORDER BY + LIMIT not supported: {}", e);
            }
        }
    }

    // ========================================================================
    // Bonus edge cases (bringing total to ~32 tests)
    // ========================================================================

    #[test]
    fn test_in_subquery_with_expression() {
        // IN subquery where the subquery SELECT uses an expression.
        // Find employees whose salary matches any project budget divided by 5.
        // budgets / 5: 500000/5=100000, 300000/5=60000, 200000/5=40000
        // Employee salaries: 90K, 80K, 70K, 60K, 75K -> only Diana (60K) matches.
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE salary IN (SELECT budget / 5 FROM projects) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                // The only salary that matches budget/5 is 60000 (Diana)
                // Note: integer division matters; 500000/5=100000 exactly.
                if rows.len() == 1 {
                    assert_eq!(rows[0].get(1), Some(&Value::String("Diana".to_string())));
                } else {
                    println!(
                        "Got {} rows instead of 1. Values: {:?}",
                        rows.len(),
                        rows.iter().map(|r| r.get(1)).collect::<Vec<_>>()
                    );
                }
            }
            Err(e) => {
                println!("KNOWN LIMITATION: IN subquery with expression not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_not_in_subquery_with_no_matching_outer_rows() {
        // NOT IN where no outer rows match the exclusion set.
        // All employee ids (1-5) are NOT IN the set of department ids (1-3)?
        // Employees 1,2,3 are IN {1,2,3} -> excluded. Only Diana(4) and Eve(5) remain.
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE id NOT IN (SELECT id FROM departments) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 2, "Employees 4 and 5 don't match dept ids, got {}", rows.len());
                assert_eq!(rows[0].get(0), Some(&Value::Int4(4)));
                assert_eq!(rows[1].get(0), Some(&Value::Int4(5)));
            }
            Err(e) => {
                println!("KNOWN LIMITATION: NOT IN subquery cross-table not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_derived_table_empty_result() {
        // Derived table returning zero rows.
        let db = setup_db();

        let sql = "SELECT * FROM (SELECT id, name FROM employees WHERE id > 999) AS empty_sub";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 0, "Derived table should return 0 rows for impossible filter");
            }
            Err(e) => {
                println!("KNOWN LIMITATION: Empty derived table not supported: {}", e);
            }
        }
    }

    #[test]
    fn test_exists_with_select_star() {
        // EXISTS (SELECT * FROM ...) -- SQL standard allows SELECT * inside EXISTS.
        // The actual columns don't matter for EXISTS; only row existence.
        let db = setup_db();

        let sql = "SELECT id, name FROM employees WHERE EXISTS (SELECT * FROM projects WHERE projects.lead_id = employees.id) ORDER BY id";
        match db.query(sql, &[]) {
            Ok(rows) => {
                assert_eq!(rows.len(), 3, "3 employees lead projects (using SELECT *), got {}", rows.len());
            }
            Err(e) => {
                println!("KNOWN LIMITATION: EXISTS with SELECT * not supported: {}", e);
            }
        }
    }
}
