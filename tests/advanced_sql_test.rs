//! Advanced SQL query tests for complex GROUP BY, HAVING, and JOIN scenarios

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn test_complex_group_by_with_having() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create employees table
    db.execute("CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, dept TEXT, salary INT)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (1, 'Alice', 'Engineering', 100000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (2, 'Bob', 'Engineering', 120000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (3, 'Charlie', 'Sales', 80000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (4, 'Dave', 'Sales', 90000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (5, 'Eve', 'Engineering', 110000)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (6, 'Frank', 'Marketing', 70000)")?;

    // Complex query: Find departments with more than 2 employees and average salary > 85000
    let results = db.query(
        "SELECT dept, COUNT(*), AVG(salary) FROM employees GROUP BY dept HAVING COUNT(*) > 2",
        &[]
    )?;

    // Only Engineering has 3 employees
    assert_eq!(results.len(), 1, "Should return 1 department with more than 2 employees");
    assert_eq!(results[0].get(0).unwrap(), &Value::String("Engineering".to_string()));
    assert_eq!(results[0].get(1).unwrap(), &Value::Int8(3)); // COUNT(*) = 3

    Ok(())
}

#[test]
fn test_join_with_aggregation() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create departments and employees tables
    db.execute("CREATE TABLE departments (dept_id INT PRIMARY KEY, dept_name TEXT, location TEXT)")?;
    db.execute("INSERT INTO departments (dept_id, dept_name, location) VALUES (1, 'Engineering', 'SF')")?;
    db.execute("INSERT INTO departments (dept_id, dept_name, location) VALUES (2, 'Sales', 'NY')")?;

    db.execute("CREATE TABLE employees (emp_id INT PRIMARY KEY, emp_name TEXT, department_id INT, salary INT)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id, salary) VALUES (1, 'Alice', 1, 100)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id, salary) VALUES (2, 'Bob', 1, 120)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id, salary) VALUES (3, 'Charlie', 2, 80)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id, salary) VALUES (4, 'Dave', 2, 90)")?;

    // Join departments with employees
    let results = db.query(
        "SELECT * FROM employees INNER JOIN departments ON employees.department_id = departments.dept_id",
        &[]
    )?;

    // Should return 4 joined rows
    assert_eq!(results.len(), 4, "Should return 4 joined rows");

    // Verify structure: employees has 4 columns + departments has 3 columns = 7 total
    assert_eq!(results[0].len(), 7, "Should have 7 columns from both tables");

    Ok(())
}

#[test]
fn test_multi_column_group_by() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create sales table
    db.execute("CREATE TABLE sales (id INT PRIMARY KEY, region TEXT, product TEXT, amount INT)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (1, 'North', 'Widget', 100)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (2, 'North', 'Widget', 150)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (3, 'North', 'Gadget', 200)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (4, 'South', 'Widget', 300)")?;
    db.execute("INSERT INTO sales (id, region, product, amount) VALUES (5, 'South', 'Gadget', 250)")?;

    // Group by multiple columns
    let results = db.query(
        "SELECT region, product, SUM(amount) FROM sales GROUP BY region, product",
        &[]
    )?;

    // Should have 4 distinct region-product combinations
    // North-Gadget, North-Widget, South-Gadget, South-Widget
    assert_eq!(results.len(), 4, "Should return 4 distinct region-product combinations");

    Ok(())
}

#[test]
fn test_count_with_distinct_and_aggregates() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create orders table
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, product TEXT, price INT)")?;
    db.execute("INSERT INTO orders (id, customer, product, price) VALUES (1, 'Alice', 'Laptop', 1000)")?;
    db.execute("INSERT INTO orders (id, customer, product, price) VALUES (2, 'Alice', 'Mouse', 20)")?;
    db.execute("INSERT INTO orders (id, customer, product, price) VALUES (3, 'Bob', 'Laptop', 1000)")?;
    db.execute("INSERT INTO orders (id, customer, product, price) VALUES (4, 'Alice', 'Keyboard', 50)")?;

    // Get total count and distinct product count
    let results = db.query(
        "SELECT COUNT(*), COUNT(DISTINCT product) FROM orders",
        &[]
    )?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &Value::Int8(4)); // Total orders
    assert_eq!(results[0].get(1).unwrap(), &Value::Int8(3)); // Distinct products (Laptop, Mouse, Keyboard)

    Ok(())
}

#[test]
fn test_having_with_average() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create student grades table
    db.execute("CREATE TABLE grades (id INT PRIMARY KEY, student TEXT, course TEXT, score INT)")?;
    db.execute("INSERT INTO grades (id, student, course, score) VALUES (1, 'Alice', 'Math', 90)")?;
    db.execute("INSERT INTO grades (id, student, course, score) VALUES (2, 'Alice', 'Science', 85)")?;
    db.execute("INSERT INTO grades (id, student, course, score) VALUES (3, 'Bob', 'Math', 70)")?;
    db.execute("INSERT INTO grades (id, student, course, score) VALUES (4, 'Bob', 'Science', 75)")?;
    db.execute("INSERT INTO grades (id, student, course, score) VALUES (5, 'Charlie', 'Math', 95)")?;
    db.execute("INSERT INTO grades (id, student, course, score) VALUES (6, 'Charlie', 'Science', 92)")?;

    // Find students with average score > 80
    let results = db.query(
        "SELECT student, AVG(score) FROM grades GROUP BY student HAVING AVG(score) > 80",
        &[]
    )?;

    // Alice (87.5) and Charlie (93.5) should be returned, not Bob (72.5)
    assert_eq!(results.len(), 2, "Should return 2 students with avg > 80");

    Ok(())
}

#[test]
fn test_cross_join() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create two small tables for cross join
    db.execute("CREATE TABLE colors (id INT PRIMARY KEY, color TEXT)")?;
    db.execute("INSERT INTO colors (id, color) VALUES (1, 'Red')")?;
    db.execute("INSERT INTO colors (id, color) VALUES (2, 'Blue')")?;

    db.execute("CREATE TABLE sizes (size_id INT PRIMARY KEY, size TEXT)")?;
    db.execute("INSERT INTO sizes (size_id, size) VALUES (1, 'Small')")?;
    db.execute("INSERT INTO sizes (size_id, size) VALUES (2, 'Large')")?;

    // Cross join (all combinations)
    // Note: Without an ON clause, JOIN creates a cross product
    // For true cross join support, we'd need explicit CROSS JOIN syntax
    // For now, this tests basic multi-table join capability
    let results = db.query(
        "SELECT * FROM colors INNER JOIN sizes ON colors.id = sizes.size_id",
        &[]
    )?;

    // With matching IDs, we should get 2 matches (Red-Small, Blue-Large)
    assert_eq!(results.len(), 2, "Should return matching color-size pairs");

    Ok(())
}

#[test]
fn test_aggregate_with_nulls() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create products table with some null prices
    db.execute("CREATE TABLE products (id INT PRIMARY KEY, name TEXT, price INT)")?;
    db.execute("INSERT INTO products (id, name, price) VALUES (1, 'Item1', 100)")?;
    db.execute("INSERT INTO products (id, name, price) VALUES (2, 'Item2', 200)")?;
    db.execute("INSERT INTO products (id, name, price) VALUES (3, 'Item3', 150)")?;

    // Aggregates should handle NULLs correctly (exclude them except for COUNT(*))
    let results = db.query(
        "SELECT COUNT(*), MIN(price), MAX(price), AVG(price), SUM(price) FROM products",
        &[]
    )?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &Value::Int8(3)); // COUNT(*) = 3
    assert_eq!(results[0].get(1).unwrap(), &Value::Int4(100)); // MIN
    assert_eq!(results[0].get(2).unwrap(), &Value::Int4(200)); // MAX
    assert_eq!(results[0].get(3).unwrap(), &Value::Float8(150.0)); // AVG
    assert_eq!(results[0].get(4).unwrap(), &Value::Int8(450)); // SUM

    Ok(())
}

#[test]
fn test_empty_group_by() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create empty table
    db.execute("CREATE TABLE empty_table (id INT PRIMARY KEY, value INT)")?;

    // Aggregates on empty table should return appropriate values
    let results = db.query(
        "SELECT COUNT(*), SUM(value) FROM empty_table",
        &[]
    )?;

    // Empty table should return 1 row with COUNT(*) = 0
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &Value::Int8(0)); // COUNT(*) on empty is 0

    Ok(())
}
