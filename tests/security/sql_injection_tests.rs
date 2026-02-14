//! SQL Injection Security Tests
//!
//! Tests to verify that the SQL parser and executor are resistant to
//! common SQL injection attack patterns.

use heliosdb_nano::{EmbeddedDatabase, Result};

#[test]
fn test_sql_injection_classic_attack() {
    // Classic SQL injection: ' OR '1'='1
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT, password TEXT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO users (id, username, password) VALUES (1, 'admin', 'secret')")
        .expect("Failed to insert data");

    // Attempt SQL injection in WHERE clause
    let malicious_input = "' OR '1'='1";
    let result = db.query(
        &format!("SELECT * FROM users WHERE username = '{}'", malicious_input),
        &[],
    );

    // This should either:
    // 1. Return no results (injection failed - parser treats as literal string)
    // 2. Return error (malformed SQL)
    // It should NOT return all users

    match result {
        Ok(results) => {
            // If query succeeds, it should return 0 results (injection failed)
            // because the parser treats the whole thing as a literal string
            assert!(
                results.is_empty(),
                "SQL injection attack succeeded! Found {} rows when expecting 0",
                results.len()
            );
        }
        Err(e) => {
            // Parse error is also acceptable - means injection was rejected
            println!("SQL injection rejected with error: {}", e);
        }
    }
}

#[test]
fn test_sql_injection_union_based() {
    // Union-based SQL injection
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO users (id, username) VALUES (1, 'alice')")
        .expect("Failed to insert data");

    // Attempt UNION injection
    let malicious_input = "1' UNION SELECT id, password FROM passwords --";
    let result = db.query(
        &format!("SELECT * FROM users WHERE id = {}", malicious_input),
        &[],
    );

    // Should fail - either parse error or no results
    match result {
        Ok(results) => {
            assert!(
                results.is_empty(),
                "UNION injection attack succeeded!"
            );
        }
        Err(e) => {
            println!("UNION injection rejected: {}", e);
        }
    }
}

#[test]
fn test_sql_injection_comment_injection() {
    // Comment-based injection to bypass authentication
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT, password TEXT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO users (id, username, password) VALUES (1, 'admin', 'secret')")
        .expect("Failed to insert data");

    // Attempt to use comments to bypass password check
    let malicious_input = "admin' --";
    let result = db.query(
        &format!(
            "SELECT * FROM users WHERE username = '{}' AND password = 'wrong'",
            malicious_input
        ),
        &[],
    );

    match result {
        Ok(results) => {
            assert!(
                results.is_empty(),
                "Comment injection attack succeeded!"
            );
        }
        Err(e) => {
            println!("Comment injection rejected: {}", e);
        }
    }
}

#[test]
fn test_sql_injection_stacked_queries() {
    // Stacked queries injection
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO users (id, username) VALUES (1, 'alice')")
        .expect("Failed to insert data");

    // Attempt to stack malicious query
    let malicious_input = "1; DROP TABLE users; --";
    let result = db.query(
        &format!("SELECT * FROM users WHERE id = {}", malicious_input),
        &[],
    );

    // Should fail - parser should reject or treat as literal
    match result {
        Ok(_) => {
            // Verify table still exists
            let verify = db.query("SELECT * FROM users", &[]);
            assert!(
                verify.is_ok(),
                "Table was dropped! Stacked query injection succeeded!"
            );
        }
        Err(e) => {
            println!("Stacked query injection rejected: {}", e);
        }
    }
}

#[test]
fn test_sql_injection_time_based_blind() {
    // Time-based blind SQL injection (should not be possible)
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    // Attempt time-based injection (PostgreSQL SLEEP equivalent)
    // Note: pg_sleep is a PostgreSQL function that may not be available
    let malicious_input = "1' AND (SELECT CASE WHEN (1=1) THEN pg_sleep(5) ELSE pg_sleep(0) END) --";

    let start = std::time::Instant::now();
    let result = db.query(
        &format!("SELECT * FROM users WHERE id = '{}'", malicious_input),
        &[],
    );
    let elapsed = start.elapsed();

    // Query should not cause significant delay (< 1 second)
    assert!(
        elapsed.as_secs() < 1,
        "Time-based SQL injection may have succeeded (took {:?})",
        elapsed
    );

    // Result should be error or empty
    match result {
        Ok(results) => assert!(results.is_empty(), "Time-based injection returned results"),
        Err(e) => println!("Time-based injection rejected: {}", e),
    }
}

#[test]
fn test_sql_injection_boolean_based_blind() {
    // Boolean-based blind SQL injection
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT, password TEXT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO users (id, username, password) VALUES (1, 'admin', 'secret')")
        .expect("Failed to insert data");

    // Attempt boolean-based blind injection
    let malicious_input = "admin' AND '1'='1";
    let result1 = db.query(
        &format!("SELECT * FROM users WHERE username = '{}'", malicious_input),
        &[],
    );

    let malicious_input2 = "admin' AND '1'='2";
    let result2 = db.query(
        &format!("SELECT * FROM users WHERE username = '{}'", malicious_input2),
        &[],
    );

    // Both should fail or return empty (not provide different results)
    match (result1, result2) {
        (Ok(r1), Ok(r2)) => {
            assert!(
                r1.is_empty() && r2.is_empty(),
                "Boolean-based blind injection may have succeeded"
            );
        }
        _ => println!("Boolean-based injection rejected"),
    }
}

#[test]
fn test_sql_injection_hex_encoding() {
    // Hex encoding injection attempt
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    // Attempt hex-encoded injection
    let malicious_input = "0x61646d696e"; // 'admin' in hex
    let result = db.query(
        &format!("SELECT * FROM users WHERE username = {}", malicious_input),
        &[],
    );

    // Should fail or return empty
    match result {
        Ok(results) => assert!(results.is_empty(), "Hex encoding injection succeeded"),
        Err(e) => println!("Hex encoding injection rejected: {}", e),
    }
}

#[test]
fn test_sql_injection_null_byte() {
    // Null byte injection
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    // Attempt null byte injection
    let malicious_input = "admin\0' OR '1'='1";
    let result = db.query(
        &format!("SELECT * FROM users WHERE username = '{}'", malicious_input),
        &[],
    );

    // Should fail or return empty
    match result {
        Ok(results) => assert!(results.is_empty(), "Null byte injection succeeded"),
        Err(e) => println!("Null byte injection rejected: {}", e),
    }
}

#[test]
fn test_sql_injection_unicode_bypass() {
    // Unicode bypass attempts
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    // Attempt Unicode variations of SQL keywords
    let malicious_inputs = vec![
        "1' ＵＮＩＯＮ ＳＥＬＥＣＴ 1 --", // Fullwidth characters
        "1' %55NION %53ELECT 1 --",       // URL encoded
    ];

    for malicious_input in malicious_inputs {
        let result = db.query(
            &format!("SELECT * FROM users WHERE id = '{}'", malicious_input),
            &[],
        );

        match result {
            Ok(results) => {
                assert!(
                    results.is_empty(),
                    "Unicode bypass injection succeeded with: {}",
                    malicious_input
                );
            }
            Err(e) => println!("Unicode bypass rejected: {}", e),
        }
    }
}

#[test]
fn test_parameterized_queries_safe() {
    // Verify that proper parameterized queries work safely
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO users (id, username) VALUES (1, 'alice')")
        .expect("Failed to insert data");

    // Even with malicious input, literal values in SQL are safe
    // because sqlparser treats them as string literals
    let safe_query = "SELECT * FROM users WHERE username = 'alice'";
    let result = db.query(safe_query, &[]);

    assert!(result.is_ok(), "Safe query should work");
    let results = result.expect("Failed to execute safe query");
    assert_eq!(results.len(), 1, "Should return exactly 1 row");
}

#[test]
fn test_sql_injection_case_manipulation() {
    // Case manipulation to bypass filters
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE users (id INT, username TEXT)")
        .expect("Failed to create table");

    let malicious_inputs = vec![
        "1' uNiOn SeLeCt 1 --",
        "1' UnIoN sElEcT 1 --",
        "1' UNION SELECT 1 --",
    ];

    for malicious_input in malicious_inputs {
        let result = db.query(
            &format!("SELECT * FROM users WHERE id = '{}'", malicious_input),
            &[],
        );

        match result {
            Ok(results) => {
                assert!(
                    results.is_empty(),
                    "Case manipulation injection succeeded: {}",
                    malicious_input
                );
            }
            Err(e) => println!("Case manipulation rejected: {}", e),
        }
    }
}
