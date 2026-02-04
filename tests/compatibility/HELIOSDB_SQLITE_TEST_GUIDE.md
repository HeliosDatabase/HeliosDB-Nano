# HeliosDB-Lite SQLite Compatibility Test Suite Guide

Comprehensive guide for running, interpreting, and extending the HeliosDB-Lite SQLite compatibility test suite.

## Table of Contents

1. [Overview](#overview)
2. [Prerequisites](#prerequisites)
3. [Running the Test Suite](#running-the-test-suite)
4. [Interpreting Test Results](#interpreting-test-results)
5. [Adding New Tests](#adding-new-tests)
6. [Continuous Integration Setup](#continuous-integration-setup)
7. [Test Coverage Reporting](#test-coverage-reporting)
8. [Troubleshooting](#troubleshooting)

---

## Overview

This test suite validates that HeliosDB-Lite can act as a drop-in replacement for SQLite by testing:

- **All sqlite3 module functions** - Connection, cursor, execute operations
- **SQL operations** - CRUD, JOINs, aggregates, transactions
- **Concurrency** - Multi-threaded access patterns
- **Data types** - Preservation of INTEGER, REAL, TEXT, BLOB, NULL
- **Error handling** - Exception mapping and error conditions
- **Performance** - Benchmarking against native SQLite

### Test Files

```
tests/compatibility/
├── HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py  # Comprehensive compatibility tests
├── HELIOSDB_SQLITE_BENCHMARK_SUITE.py      # Performance benchmarks
└── HELIOSDB_SQLITE_TEST_GUIDE.md           # This guide
```

---

## Prerequisites

### Required Dependencies

Install all required Python packages:

```bash
# Create virtual environment (recommended)
python3 -m venv venv
source venv/bin/activate  # On Windows: venv\Scripts\activate

# Install dependencies
pip install pytest pytest-timeout pytest-xdist psutil tabulate
```

### Optional Dependencies

For enhanced features:

```bash
# For parallel test execution
pip install pytest-xdist

# For HTML reports
pip install pytest-html

# For coverage reporting
pip install pytest-cov

# For benchmark visualization
pip install matplotlib
```

### Build HeliosDB-Lite

Ensure HeliosDB-Lite binary is built:

```bash
cd /path/to/HeliosDB-Lite
cargo build --release

# Binary will be at: ./target/release/heliosdb-lite
```

---

## Running the Test Suite

### Basic Execution

Run all compatibility tests:

```bash
cd tests/compatibility
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v
```

### Selective Test Execution

Run specific test classes or methods:

```bash
# Run only connection tests
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py::TestConnectionMethods -v

# Run specific test
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py::TestConnectionMethods::test_memory_connection -v

# Run tests matching pattern
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -k "transaction" -v
```

### Parallel Execution

Run tests in parallel for faster execution:

```bash
# Use all CPU cores
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -n auto

# Use specific number of workers
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -n 4
```

### Verbose Output

Control output verbosity:

```bash
# Minimal output
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -q

# Standard output
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py

# Verbose output
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v

# Very verbose output (shows all assertions)
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -vv
```

### Running Benchmarks

Execute the benchmark suite:

```bash
# Basic benchmark run
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py

# Custom iterations
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py --iterations 1000

# Generate markdown report
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py --report-format markdown --output benchmark_report.md

# Generate JSON report
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py --report-format json --output benchmark_results.json

# Console output only
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py --report-format console
```

---

## Interpreting Test Results

### Successful Test Run

```
========================= test session starts ==========================
platform linux -- Python 3.9.7, pytest-7.4.0, pluggy-1.2.0
rootdir: /home/user/HeliosDB-Lite/tests/compatibility
collected 42 items

HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py::TestConnectionMethods::test_memory_connection PASSED [  2%]
HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py::TestConnectionMethods::test_file_connection PASSED [  4%]
...
========================== 42 passed in 12.34s ==========================
```

**Interpretation:**
- All 42 tests passed
- Total execution time: 12.34 seconds
- No failures or errors

### Failed Test Example

```
FAILED HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py::TestDataTypePreservation::test_integer_types
```

**Common Failure Reasons:**

1. **Type mismatch** - Data type not preserved correctly
2. **Missing feature** - SQL feature not implemented in HeliosDB
3. **Server not running** - HeliosDB server fixture failed to start
4. **Timeout** - Operation took too long (adjust timeout if needed)

### Test Output Sections

#### Short Traceback (`--tb=short`)

```bash
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py --tb=short
```

Shows concise error information.

#### Long Traceback (`--tb=long`)

```bash
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py --tb=long
```

Shows detailed stack traces.

#### No Traceback (`--tb=no`)

```bash
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py --tb=no
```

Shows only test results without error details.

### Benchmark Results

Sample benchmark output:

```
BENCHMARK SUMMARY
================================================================================
Benchmark            SQLite      HeliosDB    Speedup    Winner
--------------------------------------------------------------------------------
Simple SELECT        1.23ms      1.10ms      1.12x      HeliosDB
Batch INSERT         45.67ms     42.34ms     1.08x      HeliosDB
Transaction COMMIT   2.34ms      2.56ms      0.91x      SQLite
JOIN Query           12.45ms     11.23ms     1.11x      HeliosDB
================================================================================
```

**Key Metrics:**

- **Speedup > 1.0** - HeliosDB is faster
- **Speedup < 1.0** - SQLite is faster
- **Speedup ≈ 1.0** - Performance is equivalent

---

## Adding New Tests

### Test Structure

Follow pytest conventions and existing patterns:

```python
class TestNewFeature:
    """Test description"""

    def test_specific_behavior(self, memory_connection):
        """Test specific behavior description"""
        cursor = memory_connection.cursor()

        # Arrange - Set up test data
        cursor.execute("CREATE TABLE test (id INTEGER)")

        # Act - Perform the operation
        cursor.execute("INSERT INTO test VALUES (1)")

        # Assert - Verify the results
        cursor.execute("SELECT * FROM test")
        assert cursor.fetchone() == (1,)
```

### Using Fixtures

Available fixtures:

```python
def test_with_temp_file(self, temp_db_path):
    """Use temporary database file"""
    conn = sqlite3.connect(temp_db_path)
    # Test code...

def test_with_memory(self, memory_connection):
    """Use in-memory database"""
    cursor = memory_connection.cursor()
    # Test code...

def test_with_file(self, file_connection):
    """Use file-based connection"""
    cursor = file_connection.cursor()
    # Test code...

def test_with_server(self, heliosdb_server):
    """Use HeliosDB server"""
    config = heliosdb_server
    # Test code...
```

### Test Markers

Add markers to categorize tests:

```python
import pytest

@pytest.mark.slow
def test_large_dataset(self):
    """Marked as slow test"""
    pass

@pytest.mark.requires_server
def test_server_feature(self, heliosdb_server):
    """Requires HeliosDB server running"""
    pass

@pytest.mark.skip(reason="Not implemented yet")
def test_future_feature(self):
    """Skipped test"""
    pass

@pytest.mark.xfail(reason="Known issue #123")
def test_known_bug(self):
    """Expected to fail"""
    pass
```

Run tests by marker:

```bash
# Run only slow tests
pytest -m slow

# Skip slow tests
pytest -m "not slow"

# Run tests requiring server
pytest -m requires_server
```

### Adding Benchmark Tests

Add new benchmark to `HELIOSDB_SQLITE_BENCHMARK_SUITE.py`:

```python
@staticmethod
def benchmark_custom_operation(config: BenchmarkConfig) -> BenchmarkResult:
    """Benchmark custom operation"""
    harness = BenchmarkHarness(config)

    def setup():
        # Setup code
        conn = sqlite3.connect(":memory:")
        # ... initialize test data
        return {"conn": conn, "cursor": cursor}

    def benchmark(ctx):
        # Code to benchmark
        ctx["cursor"].execute("YOUR SQL HERE")
        ctx["cursor"].fetchall()

    def teardown(ctx):
        # Cleanup code
        ctx["conn"].close()

    return harness.run_benchmark(
        "Custom Operation",
        "sqlite",
        setup,
        benchmark,
        teardown
    )
```

Then add to benchmark list in `run_all_benchmarks()`:

```python
benchmarks = [
    # ... existing benchmarks
    SQLiteBenchmarks.benchmark_custom_operation,
]
```

---

## Continuous Integration Setup

### GitHub Actions

Create `.github/workflows/sqlite-compatibility-tests.yml`:

```yaml
name: SQLite Compatibility Tests

on:
  push:
    branches: [ main, develop ]
  pull_request:
    branches: [ main ]

jobs:
  test:
    runs-on: ubuntu-latest

    steps:
    - uses: actions/checkout@v3

    - name: Set up Python
      uses: actions/setup-python@v4
      with:
        python-version: '3.9'

    - name: Install dependencies
      run: |
        python -m pip install --upgrade pip
        pip install pytest pytest-xdist pytest-cov psutil tabulate

    - name: Build HeliosDB
      run: |
        cargo build --release

    - name: Run compatibility tests
      run: |
        cd tests/compatibility
        pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v --tb=short

    - name: Run benchmarks
      run: |
        cd tests/compatibility
        python HELIOSDB_SQLITE_BENCHMARK_SUITE.py --iterations 50 --output benchmark_report.md

    - name: Upload benchmark results
      uses: actions/upload-artifact@v3
      with:
        name: benchmark-report
        path: tests/compatibility/benchmark_report.md
```

### GitLab CI

Create `.gitlab-ci.yml`:

```yaml
sqlite-tests:
  stage: test
  image: rust:latest
  before_script:
    - apt-get update && apt-get install -y python3 python3-pip
    - pip3 install pytest pytest-xdist psutil tabulate
  script:
    - cargo build --release
    - cd tests/compatibility
    - pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v
  artifacts:
    reports:
      junit: tests/compatibility/junit.xml
```

### Jenkins Pipeline

Create `Jenkinsfile`:

```groovy
pipeline {
    agent any

    stages {
        stage('Build') {
            steps {
                sh 'cargo build --release'
            }
        }

        stage('Test') {
            steps {
                sh '''
                    pip install pytest pytest-xdist psutil tabulate
                    cd tests/compatibility
                    pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v --junitxml=junit.xml
                '''
            }
        }
    }

    post {
        always {
            junit 'tests/compatibility/junit.xml'
        }
    }
}
```

---

## Test Coverage Reporting

### Generate Coverage Report

```bash
# Run tests with coverage
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py --cov=. --cov-report=html

# View HTML report
open htmlcov/index.html  # macOS
xdg-open htmlcov/index.html  # Linux
start htmlcov/index.html  # Windows
```

### Coverage Output Formats

```bash
# Terminal output
pytest --cov=. --cov-report=term

# HTML report
pytest --cov=. --cov-report=html

# XML report (for CI systems)
pytest --cov=. --cov-report=xml

# Multiple formats
pytest --cov=. --cov-report=term --cov-report=html --cov-report=xml
```

### Coverage Thresholds

Fail if coverage is below threshold:

```bash
pytest --cov=. --cov-fail-under=80
```

### Configuration File

Create `pytest.ini`:

```ini
[pytest]
testpaths = tests/compatibility
python_files = HELIOSDB_SQLITE_*.py
python_classes = Test*
python_functions = test_*

# Coverage settings
addopts =
    -v
    --tb=short
    --cov=.
    --cov-report=term-missing
    --cov-fail-under=80

# Markers
markers =
    slow: marks tests as slow (deselect with '-m "not slow"')
    requires_server: requires HeliosDB server running
    integration: integration tests
    benchmark: performance benchmarks
```

---

## Troubleshooting

### Common Issues

#### 1. Server Fails to Start

**Symptom:**
```
ERROR: HeliosDB server failed to start within timeout
```

**Solutions:**
- Check if port 5432 is already in use: `lsof -i :5432`
- Increase startup timeout in fixture
- Build HeliosDB in release mode: `cargo build --release`
- Check HeliosDB logs for errors

#### 2. Import Errors

**Symptom:**
```
ModuleNotFoundError: No module named 'pytest'
```

**Solution:**
```bash
pip install pytest pytest-xdist pytest-timeout psutil tabulate
```

#### 3. Permission Denied

**Symptom:**
```
PermissionError: [Errno 13] Permission denied: './test-heliosdb-data'
```

**Solution:**
```bash
# Clean up test directories
rm -rf test-heliosdb-data test-*.db

# Check file permissions
chmod -R 755 tests/compatibility
```

#### 4. Tests Hanging

**Symptom:**
Tests appear to hang without completing

**Solutions:**
- Use timeout marker: `@pytest.mark.timeout(30)`
- Kill zombie processes: `pkill -9 heliosdb-lite`
- Check for database locks: `fuser test.db`

#### 5. Flaky Tests

**Symptom:**
Tests pass sometimes, fail other times

**Solutions:**
- Add sleep/wait for async operations
- Use proper synchronization (locks, events)
- Clean up resources in teardown
- Isolate test data (use unique temp files)

### Debug Mode

Enable verbose debugging:

```bash
# Maximum verbosity
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -vv --log-cli-level=DEBUG

# Show print statements
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -s

# Drop into debugger on failure
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py --pdb

# Show local variables on failure
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -l
```

### Performance Issues

If tests are slow:

```bash
# Run in parallel
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -n auto

# Skip slow tests
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -m "not slow"

# Profile test execution
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py --durations=10
```

### Getting Help

If you encounter issues:

1. Check this guide's troubleshooting section
2. Review test output with `-vv` for details
3. Search existing GitHub issues
4. Open a new issue with:
   - Full test output
   - System information (OS, Python version)
   - HeliosDB version
   - Steps to reproduce

---

## Best Practices

### Writing Tests

1. **One assertion per test** - Tests should be focused and specific
2. **Descriptive names** - Test names should clearly describe what they test
3. **Arrange-Act-Assert** - Structure tests with clear setup, execution, and verification
4. **Clean up resources** - Always close connections and clean up temp files
5. **Use fixtures** - Leverage pytest fixtures for common setup

### Running Tests

1. **Run locally first** - Test changes locally before CI
2. **Use parallel execution** - Speed up test runs with `-n auto`
3. **Check coverage** - Ensure new code is tested
4. **Run benchmarks regularly** - Track performance regressions
5. **Review failures carefully** - Don't ignore flaky tests

### Maintaining Tests

1. **Keep tests up to date** - Update tests when features change
2. **Remove obsolete tests** - Delete tests for removed features
3. **Document complex tests** - Add comments explaining non-obvious logic
4. **Refactor duplicated code** - Extract common patterns to fixtures
5. **Monitor test performance** - Keep test execution time reasonable

---

## Additional Resources

- [pytest Documentation](https://docs.pytest.org/)
- [SQLite Documentation](https://www.sqlite.org/docs.html)
- [Python sqlite3 Module](https://docs.python.org/3/library/sqlite3.html)
- [HeliosDB Documentation](../../docs/README.md)

---

## Appendix: Complete Test Matrix

### Test Coverage Matrix

| Category | Test Class | Tests | Status |
|----------|-----------|-------|--------|
| Connection | TestConnectionMethods | 6 | ✓ |
| Execute | TestExecuteOperations | 6 | ✓ |
| Cursor | TestCursorOperations | 7 | ✓ |
| Data Types | TestDataTypePreservation | 6 | ✓ |
| CRUD | TestCRUDOperations | 6 | ✓ |
| Joins | TestJoinsAndAggregations | 5 | ✓ |
| Transactions | TestTransactions | 4 | ✓ |
| Errors | TestErrorHandling | 6 | ✓ |
| Unicode | TestUnicodeAndEncoding | 3 | ✓ |
| Large Data | TestLargeDatasets | 4 | ✓ |
| Concurrency | TestConcurrentAccess | 2 | ✓ |
| Memory | TestMemoryMode | 2 | ✓ |

**Total Tests: 57**

### Benchmark Coverage

| Benchmark | Metrics | Status |
|-----------|---------|--------|
| Simple SELECT | Latency, Throughput | ✓ |
| Batch INSERT | Latency, Throughput, Memory | ✓ |
| Transaction COMMIT | Latency, Throughput | ✓ |
| JOIN Query | Latency, Throughput | ✓ |
| Aggregate Query | Latency, Throughput | ✓ |
| Large Result Set | Latency, Memory | ✓ |
| Concurrent Reads | Latency, Scaling | ✓ |

**Total Benchmarks: 7**

---

*Last Updated: 2025-12-08*
*Version: 1.0.0*
