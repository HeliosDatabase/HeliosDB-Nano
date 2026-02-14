//! Tests for LATERAL joins and subqueries in FROM clause
//!
//! Tests subqueries in FROM clause (derived tables)

use heliosdb_nano::{Config, EmbeddedDatabase};

fn create_test_db() -> EmbeddedDatabase {
    let config = Config::in_memory();
    EmbeddedDatabase::with_config(config).unwrap()
}

#[test]
fn test_subquery_in_from_simple() {
    let db = create_test_db();

    // Create tables with data
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, amount INT)").unwrap();
    db.execute("INSERT INTO orders (id, amount) VALUES (1, 100), (2, 200), (3, 300)").unwrap();

    // Test simple subquery in FROM clause without alias qualification
    let results = db.query("SELECT * FROM (SELECT id, amount FROM orders) AS sub WHERE amount > 150", &[]).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_cross_join_with_subquery() {
    let db = create_test_db();

    // Create tables
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob')").unwrap();

    db.execute("CREATE TABLE products (id INT PRIMARY KEY, pname TEXT, price INT)").unwrap();
    db.execute("INSERT INTO products (id, pname, price) VALUES (1, 'Widget', 10), (2, 'Gadget', 20)").unwrap();

    // Test cross join with subquery - use CROSS JOIN instead of comma
    let results = db.query(
        "SELECT name, pname FROM users CROSS JOIN (SELECT pname FROM products) AS p",
        &[]
    ).unwrap();
    // 2 users x 2 products = 4 rows
    assert_eq!(results.len(), 4);
}

#[test]
fn test_inner_join_with_subquery() {
    let db = create_test_db();

    // Create tables
    db.execute("CREATE TABLE departments (dept_id INT PRIMARY KEY, dept_name TEXT)").unwrap();
    db.execute("INSERT INTO departments (dept_id, dept_name) VALUES (1, 'Engineering'), (2, 'Sales')").unwrap();

    db.execute("CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, dept_id INT)").unwrap();
    db.execute("INSERT INTO employees (id, name, dept_id) VALUES (1, 'Alice', 1), (2, 'Bob', 1), (3, 'Charlie', 2)").unwrap();

    // Test join with subquery - use column names that don't need qualification
    let results = db.query(
        "SELECT name, dept_name FROM employees JOIN (SELECT dept_id AS d_id, dept_name FROM departments) AS d ON dept_id = d_id",
        &[]
    ).unwrap();
    assert_eq!(results.len(), 3);
}

#[test]
fn test_nested_subqueries() {
    let db = create_test_db();

    // Create a table
    db.execute("CREATE TABLE data (id INT PRIMARY KEY, value INT)").unwrap();
    db.execute("INSERT INTO data (id, value) VALUES (1, 10), (2, 20), (3, 30)").unwrap();

    // Test nested subqueries
    let results = db.query(
        "SELECT * FROM (SELECT * FROM (SELECT id, value FROM data) AS inner1) AS outer1 WHERE value > 15",
        &[]
    ).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_subquery_with_aggregation() {
    let db = create_test_db();

    // Create tables
    db.execute("CREATE TABLE sales (id INT PRIMARY KEY, product TEXT, amount INT)").unwrap();
    db.execute("INSERT INTO sales (id, product, amount) VALUES (1, 'A', 100), (2, 'A', 200), (3, 'B', 150)").unwrap();

    // Test subquery with aggregation - use column names without alias
    let results = db.query(
        "SELECT * FROM (SELECT product, SUM(amount) AS total FROM sales GROUP BY product) AS s WHERE total > 200",
        &[]
    ).unwrap();
    assert_eq!(results.len(), 1); // Only product A with total 300
}

#[test]
fn test_subquery_with_filter() {
    let db = create_test_db();

    // Create table
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, category TEXT, price INT)").unwrap();
    db.execute("INSERT INTO items (id, category, price) VALUES (1, 'A', 10), (2, 'A', 20), (3, 'B', 30)").unwrap();

    // Test subquery that filters data
    let results = db.query(
        "SELECT * FROM (SELECT * FROM items WHERE category = 'A') AS filtered",
        &[]
    ).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_subquery_select_specific_columns() {
    let db = create_test_db();

    // Create table
    db.execute("CREATE TABLE records (id INT PRIMARY KEY, col1 TEXT, col2 INT, col3 TEXT)").unwrap();
    db.execute("INSERT INTO records (id, col1, col2, col3) VALUES (1, 'a', 10, 'x'), (2, 'b', 20, 'y')").unwrap();

    // Test subquery that selects specific columns
    let results = db.query(
        "SELECT col1, col2 FROM (SELECT col1, col2 FROM records) AS sub",
        &[]
    ).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values.len(), 2); // Only 2 columns
}
