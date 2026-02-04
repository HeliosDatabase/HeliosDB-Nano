# SQLite Compatibility Test Suite - Quick Reference Card

## Essential Commands

### Installation

```bash
# Install dependencies
pip install -r requirements.txt

# Build HeliosDB
cd ../.. && cargo build --release
```

### Run Tests

```bash
# All tests
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v

# Specific test class
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py::TestConnectionMethods -v

# Tests matching pattern
pytest -k "transaction" -v

# Parallel execution
pytest -n auto

# With coverage
pytest --cov=. --cov-report=html
```

### Run Benchmarks

```bash
# Basic
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py

# Custom iterations
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py --iterations 1000

# Generate report
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py --report-format markdown --output report.md
```

### Automated Runner

```bash
# All tests
./run_tests.sh

# With benchmarks
./run_tests.sh -b

# Coverage + parallel
./run_tests.sh -c -p

# Benchmarks only
./run_tests.sh --bench-only -i 500
```

## Test Markers

```bash
# Run slow tests only
pytest -m slow

# Skip slow tests
pytest -m "not slow"

# Integration tests
pytest -m integration

# Server-dependent tests
pytest -m requires_server
```

## Common Options

```bash
# Verbose
pytest -vv

# Show print output
pytest -s

# Stop on first failure
pytest -x

# Drop to debugger on failure
pytest --pdb

# Show duration of slowest tests
pytest --durations=10

# Generate JUnit XML
pytest --junitxml=results.xml

# Generate HTML report
pytest --html=report.html
```

## File Reference

| File | Purpose |
|------|---------|
| HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py | Main test suite (57 tests) |
| HELIOSDB_SQLITE_BENCHMARK_SUITE.py | Performance benchmarks (7 tests) |
| HELIOSDB_SQLITE_TEST_GUIDE.md | Comprehensive documentation |
| EXAMPLE_TEST_TEMPLATE.py | Template for new tests |
| run_tests.sh | Automated test runner |
| pytest.ini | Pytest configuration |
| requirements.txt | Python dependencies |

## Test Categories

- **Connection** (6) - Connection methods and parameters
- **Execute** (6) - execute, executemany, executescript
- **Cursor** (7) - fetch, iteration, attributes
- **Data Types** (6) - INTEGER, REAL, TEXT, BLOB, NULL
- **CRUD** (6) - CREATE, INSERT, SELECT, UPDATE, DELETE
- **Joins** (5) - INNER/LEFT JOIN, aggregates
- **Transactions** (4) - COMMIT, ROLLBACK, SAVEPOINT
- **Errors** (6) - Exception handling
- **Unicode** (3) - International text support
- **Large Data** (4) - Performance with large datasets
- **Concurrent** (2) - Multi-threaded access
- **Memory** (2) - In-memory databases

## Documentation

- **HELIOSDB_SQLITE_TEST_GUIDE.md** - Full guide
- **README.md** - Quick start
- **TEST_SUITE_SUMMARY.md** - Detailed summary
- **EXAMPLE_TEST_TEMPLATE.py** - Code examples

## CI/CD Integration

```yaml
# GitHub Actions
- run: |
    pip install -r requirements.txt
    pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v
```

## Troubleshooting

| Issue | Solution |
|-------|----------|
| Import errors | `pip install -r requirements.txt` |
| Server won't start | Check port 5432, build in release mode |
| Tests hang | Use `--timeout=30`, kill zombie processes |
| Permission denied | `chmod -R 755 .`, remove test directories |

## Coverage Targets

- Line Coverage: >80%
- Branch Coverage: >75%
- Test Pass Rate: 100%
- Performance: Within 2x of SQLite

---

**Quick Help:** `pytest --help` or see HELIOSDB_SQLITE_TEST_GUIDE.md
