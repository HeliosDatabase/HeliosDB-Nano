//! Test helper utilities for HeliosDB Lite integration tests
//!
//! This module provides:
//! - Test data generators
//! - Common test fixtures
//! - Test utilities and assertions

use heliosdb_nano::{EmbeddedDatabase, Result, Value, Tuple};
use rand::Rng;

/// Test data generator
pub struct TestDataGenerator {
    rng: rand::rngs::ThreadRng,
}

impl TestDataGenerator {
    /// Create a new test data generator
    pub fn new() -> Self {
        Self {
            rng: rand::thread_rng(),
        }
    }

    /// Generate random string of given length
    pub fn random_string(&mut self, length: usize) -> String {
        (0..length)
            .map(|_| self.rng.sample(rand::distributions::Alphanumeric) as char)
            .collect()
    }

    /// Generate random integer
    pub fn random_int(&mut self, min: i32, max: i32) -> i32 {
        self.rng.gen_range(min..=max)
    }

    /// Generate random email
    pub fn random_email(&mut self) -> String {
        format!("{}@example.com", self.random_string(10).to_lowercase())
    }

    /// Generate random name
    pub fn random_name(&mut self) -> String {
        let first_names = ["Alice", "Bob", "Charlie", "Dave", "Eve", "Frank", "Grace", "Henry"];
        let last_names = ["Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller"];

        let first = first_names[self.rng.gen_range(0..first_names.len())];
        let last = last_names[self.rng.gen_range(0..last_names.len())];

        format!("{} {}", first, last)
    }

    /// Generate realistic test user data
    pub fn generate_user(&mut self, id: i32) -> (i32, String, String, i32) {
        let name = self.random_name();
        let email = self.random_email();
        let age = self.rng.gen_range(18..80);
        (id, name, email, age)
    }

    /// Generate batch of users
    pub fn generate_users(&mut self, count: usize) -> Vec<(i32, String, String, i32)> {
        (1..=count as i32)
            .map(|id| self.generate_user(id))
            .collect()
    }
}

impl Default for TestDataGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Setup a test database with users table
pub fn setup_users_table(db: &EmbeddedDatabase) -> Result<()> {
    // Try to create table, ignore error if it already exists
    match db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, email TEXT, age INT)") {
        Ok(_) => Ok(()),
        Err(e) => {
            // Ignore "table already exists" errors
            let err_str = e.to_string();
            if err_str.contains("already exists") {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Setup and populate a test database with sample data
pub fn setup_with_test_data(db: &EmbeddedDatabase, count: usize) -> Result<()> {
    setup_users_table(db)?;

    let mut gen = TestDataGenerator::new();
    let users = gen.generate_users(count);

    for (id, name, email, age) in users {
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, '{}', '{}', {})",
            id, name, email, age
        ))?;
    }

    Ok(())
}

/// Setup products and orders tables for join tests
pub fn setup_ecommerce_schema(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE products (product_id INT PRIMARY KEY, name TEXT, price INT)")?;
    db.execute("CREATE TABLE orders (order_id INT PRIMARY KEY, product_id INT, customer TEXT, quantity INT)")?;
    db.execute("CREATE TABLE customers (customer_id INT PRIMARY KEY, name TEXT, email TEXT)")?;
    Ok(())
}

/// Assert that a query returns expected number of rows
pub fn assert_row_count(db: &EmbeddedDatabase, query: &str, expected: usize) -> Result<()> {
    let results = db.query(query, &[])?;
    assert_eq!(
        results.len(),
        expected,
        "Expected {} rows but got {}",
        expected,
        results.len()
    );
    Ok(())
}

/// Assert that a value matches expected
pub fn assert_value_eq(actual: &Value, expected: &Value, context: &str) {
    assert_eq!(
        actual, expected,
        "{}: Expected {:?} but got {:?}",
        context, expected, actual
    );
}

/// Extract integer value from result
pub fn get_int_value(tuple: &Tuple, index: usize) -> Option<i64> {
    match tuple.get(index)? {
        Value::Int2(i) => Some(*i as i64),
        Value::Int4(i) => Some(*i as i64),
        Value::Int8(i) => Some(*i),
        _ => None,
    }
}

/// Extract string value from result
pub fn get_string_value(tuple: &Tuple, index: usize) -> Option<String> {
    match tuple.get(index)? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Extract boolean value from result
pub fn get_bool_value(tuple: &Tuple, index: usize) -> Option<bool> {
    match tuple.get(index)? {
        Value::Boolean(b) => Some(*b),
        _ => None,
    }
}

/// Extract float value from result
pub fn get_float_value(tuple: &Tuple, index: usize) -> Option<f64> {
    match tuple.get(index)? {
        Value::Float4(f) => Some(*f as f64),
        Value::Float8(f) => Some(*f),
        _ => None,
    }
}

/// Measure query execution time
pub fn measure_query_time<F>(f: F) -> std::time::Duration
where
    F: FnOnce() -> Result<()>,
{
    let start = std::time::Instant::now();
    f().expect("Query failed");
    start.elapsed()
}

/// Performance assertion - query should complete within time limit
pub fn assert_query_performance<F>(f: F, max_duration: std::time::Duration, description: &str)
where
    F: FnOnce() -> Result<()>,
{
    let duration = measure_query_time(f);
    assert!(
        duration <= max_duration,
        "{} took {:?} but should complete within {:?}",
        description,
        duration,
        max_duration
    );
}

/// Create a test database with common configuration
pub fn create_test_db() -> Result<EmbeddedDatabase> {
    EmbeddedDatabase::new_in_memory()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_generator() {
        let mut gen = TestDataGenerator::new();

        // Test string generation
        let s = gen.random_string(10);
        assert_eq!(s.len(), 10);

        // Test int generation
        let i = gen.random_int(1, 10);
        assert!(i >= 1 && i <= 10);

        // Test email generation
        let email = gen.random_email();
        assert!(email.contains("@example.com"));

        // Test user generation
        let users = gen.generate_users(5);
        assert_eq!(users.len(), 5);
    }

    #[test]
    fn test_setup_helpers() -> Result<()> {
        let db = create_test_db()?;
        setup_users_table(&db)?;

        // Verify table was created
        db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Test', 'test@example.com', 25)")?;
        let results = db.query("SELECT * FROM users", &[])?;
        assert_eq!(results.len(), 1);

        Ok(())
    }

    #[test]
    fn test_value_extractors() -> Result<()> {
        let db = create_test_db()?;
        setup_users_table(&db)?;
        db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

        let results = db.query("SELECT * FROM users", &[])?;
        let row = &results[0];

        assert_eq!(get_int_value(row, 0), Some(1));
        assert_eq!(get_string_value(row, 1), Some("Alice".to_string()));
        assert_eq!(get_string_value(row, 2), Some("alice@example.com".to_string()));
        assert_eq!(get_int_value(row, 3), Some(30));

        Ok(())
    }
}
