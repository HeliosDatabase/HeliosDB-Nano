# Security Test Suite

This directory contains comprehensive security tests for HeliosDB Lite.

## Test Coverage

### 1. SQL Injection Tests (`sql_injection_tests.rs`)

Tests to verify that the database is resistant to SQL injection attacks:

- **Classic SQL Injection**: `' OR '1'='1` patterns
- **Union-Based Injection**: UNION SELECT attacks
- **Comment Injection**: `admin' --` bypass attempts
- **Stacked Queries**: `1; DROP TABLE users;` attacks
- **Time-Based Blind Injection**: Timing-based inference attacks
- **Boolean-Based Blind Injection**: Logic-based inference attacks
- **Encoding Attacks**: Hex encoding, URL encoding
- **Null Byte Injection**: `\0` character bypass attempts
- **Unicode Bypass**: Fullwidth characters, special encodings
- **Case Manipulation**: Mixed case keyword variations

**Total Tests:** 11

### 2. Resource Exhaustion Tests (`resource_exhaustion_tests.rs`)

Tests to verify the database handles resource-intensive operations gracefully:

- **Large Result Sets**: 10,000+ row queries
- **Deeply Nested Queries**: Stack depth testing
- **Long String Insertion**: 1 MB+ string handling
- **Cartesian Products**: Large join operations
- **Multiple Joins**: Complex multi-table joins
- **Complex Aggregates**: Heavy GROUP BY operations
- **Rapid Connection Creation**: Connection pooling stress
- **Concurrent Queries**: Multi-threaded stress testing
- **Memory Limits**: Memory usage validation
- **Division by Zero**: Error handling verification

**Total Tests:** 10

### 3. Cryptography Tests (`crypto_tests.rs`)

Tests to verify cryptographic implementation correctness and security:

- **Roundtrip Testing**: Encrypt/decrypt validation
- **Nonce Uniqueness**: Random nonce verification
- **Wrong Key Detection**: Authentication testing
- **Tamper Detection**: AEAD property verification
- **Edge Cases**: Empty data, large data handling
- **Key Derivation**: Argon2 consistency testing
- **Password Variations**: Different password handling
- **Salt Variations**: Different salt handling
- **Unicode Support**: International character handling
- **Binary Data**: Arbitrary byte handling
- **Weak Passwords**: Edge case handling
- **Concurrent Operations**: Thread safety testing
- **Performance Baseline**: Performance monitoring

**Total Tests:** 18

## Running Tests

### Run All Security Tests

```bash
# From heliosdb-lite directory
cargo test --test sql_injection_tests --test resource_exhaustion_tests --test crypto_tests
```

### Run Individual Test Suites

```bash
# SQL injection tests only
cargo test --test sql_injection_tests -- --nocapture

# Resource exhaustion tests only
cargo test --test resource_exhaustion_tests -- --nocapture

# Cryptography tests only
cargo test --test crypto_tests -- --nocapture
```

### Run Specific Test

```bash
# Run a specific test by name
cargo test --test sql_injection_tests test_sql_injection_classic_attack -- --nocapture
```

## Test Philosophy

### Defense in Depth

These tests assume that security is implemented in layers:

1. **Parser Layer**: sqlparser-rs validates SQL syntax
2. **Planner Layer**: Logical planning validates operations
3. **Executor Layer**: Physical execution validates data
4. **Storage Layer**: Data integrity and encryption

### Expected Behaviors

Tests verify these security properties:

1. **Fail Safely**: Attacks should fail without compromising system
2. **No Information Leakage**: Error messages don't reveal internals
3. **Graceful Degradation**: System remains responsive after attacks
4. **Resource Limits**: No unbounded resource consumption
5. **Cryptographic Strength**: Proper use of proven algorithms

## Test Results Interpretation

### Passing Tests

A passing test means:
- Attack was detected and blocked, OR
- Attack failed due to input validation, OR
- System handled edge case correctly

### Failing Tests

A failing test indicates:
- Security vulnerability exists
- Resource limit missing
- Error handling inadequate
- Cryptographic implementation flaw

## Adding New Tests

When adding security tests:

1. **Document the Attack**: Explain what you're testing
2. **Show Expected Behavior**: Document pass/fail criteria
3. **Include Edge Cases**: Test boundary conditions
4. **Verify Graceful Failure**: System should remain stable
5. **Add to CI/CD**: Update workflow if needed

### Example Test Template

```rust
#[test]
fn test_new_attack_vector() {
    // Setup
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create database");

    db.execute("CREATE TABLE test (id INT)")
        .expect("Failed to create table");

    // Attempt attack
    let malicious_input = "attack payload";
    let result = db.query(
        &format!("SELECT * FROM test WHERE id = '{}'", malicious_input),
        &[],
    );

    // Verify attack failed
    match result {
        Ok(results) => {
            assert!(results.is_empty(), "Attack should not return data");
        }
        Err(e) => {
            println!("Attack rejected: {}", e);
        }
    }

    // Verify system is still responsive
    let verify = db.query("SELECT * FROM test", &[]);
    assert!(verify.is_ok(), "Database should remain responsive");
}
```

## CI/CD Integration

These tests are automatically run in:

- **PR Checks**: Every pull request
- **Daily Builds**: Every day at 2 AM UTC
- **Security Workflow**: On security-related changes

See `.github/workflows/security-tests.yml` for details.

## Performance Considerations

Some tests intentionally stress the system:

- **Large Data Tests**: May take 5-10 seconds
- **Concurrent Tests**: Spawn multiple threads
- **Memory Tests**: Allocate significant memory

Run with `--test-threads=1` if needed:

```bash
cargo test --test resource_exhaustion_tests -- --test-threads=1
```

## Security Test Metrics

Current coverage:

| Category | Tests | Coverage |
|----------|-------|----------|
| SQL Injection | 11 | Excellent |
| Resource Exhaustion | 10 | Good |
| Cryptography | 18 | Excellent |
| **Total** | **39** | **Excellent** |

## References

- [OWASP SQL Injection](https://owasp.org/www-community/attacks/SQL_Injection)
- [CWE-89: SQL Injection](https://cwe.mitre.org/data/definitions/89.html)
- [NIST Cryptographic Standards](https://csrc.nist.gov/projects/cryptographic-standards-and-guidelines)

## Reporting Security Issues

If these tests reveal a vulnerability:

1. **Do not create a public issue**
2. Email security@heliosdb.io
3. Include test case demonstrating the issue
4. We'll respond within 48 hours

See `SECURITY.md` for full reporting guidelines.

---

**Last Updated:** 2025-11-13
**Test Suite Version:** 1.0
**Total Tests:** 39
**Coverage:** Excellent
