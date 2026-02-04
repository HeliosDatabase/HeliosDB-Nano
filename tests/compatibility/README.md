# HeliosDB-Lite SQLite Compatibility Test Suite

Comprehensive test suite validating HeliosDB-Lite as a drop-in SQLite replacement.

## Quick Start

### Installation

```bash
# Install dependencies
pip install -r requirements.txt

# Build HeliosDB-Lite
cd ../..
cargo build --release
```

### Run Tests

```bash
# Run all compatibility tests
pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v

# Run benchmarks
python HELIOSDB_SQLITE_BENCHMARK_SUITE.py
```

## Files

- **HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py** (2400+ tokens) - Comprehensive pytest test suite
  - 57 tests covering all SQLite functionality
  - Connection, execute, cursor operations
  - Data type preservation
  - CRUD, JOINs, aggregates
  - Transactions, error handling
  - Unicode, large datasets
  - Concurrent access
  - Memory and server modes

- **HELIOSDB_SQLITE_BENCHMARK_SUITE.py** (1200+ tokens) - Performance benchmarks
  - 7 benchmark tests
  - SQLite vs HeliosDB comparison
  - Latency and throughput measurements
  - Memory usage tracking
  - Markdown/JSON report generation

- **HELIOSDB_SQLITE_TEST_GUIDE.md** (900+ tokens) - Complete testing guide
  - Running tests
  - Interpreting results
  - Adding new tests
  - CI/CD integration
  - Coverage reporting
  - Troubleshooting

## Test Coverage

### Categories

- **Connection Tests** (6 tests) - All connection methods and parameters
- **Execute Operations** (6 tests) - execute, executemany, executescript
- **Cursor Operations** (7 tests) - fetch methods, iteration, attributes
- **Data Type Preservation** (6 tests) - INTEGER, REAL, TEXT, BLOB, NULL, boolean
- **CRUD Operations** (6 tests) - CREATE, INSERT, SELECT, UPDATE, DELETE
- **JOIN & Aggregates** (5 tests) - INNER/LEFT JOIN, COUNT/SUM/AVG/MIN/MAX, GROUP BY, HAVING
- **Transactions** (4 tests) - COMMIT, ROLLBACK, SAVEPOINT, nested transactions
- **Error Handling** (6 tests) - Syntax errors, constraints, foreign keys
- **Unicode & Encoding** (3 tests) - Unicode text, column names, encoding consistency
- **Large Datasets** (4 tests) - Batch inserts, large results, text/blob fields
- **Concurrent Access** (2 tests) - Concurrent reads/writes
- **Memory Mode** (2 tests) - In-memory databases, shared memory

**Total: 57 tests**

### Benchmarks

1. Simple SELECT - Basic query latency
2. Batch INSERT - Bulk write throughput
3. Transaction COMMIT - Transaction overhead
4. JOIN Query - Complex query performance
5. Aggregate Query - Aggregation performance
6. Large Result Set - Memory efficiency
7. Concurrent Reads - Scalability

**Total: 7 benchmarks**

## Documentation

See [HELIOSDB_SQLITE_TEST_GUIDE.md](./HELIOSDB_SQLITE_TEST_GUIDE.md) for:
- Detailed usage instructions
- Test interpretation
- Adding new tests
- CI/CD setup
- Coverage reporting
- Troubleshooting

## CI/CD Integration

### GitHub Actions Example

```yaml
- name: Run SQLite Compatibility Tests
  run: |
    cd tests/compatibility
    pip install -r requirements.txt
    pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v
```

See the full guide for GitLab CI, Jenkins, and other CI systems.

## Requirements

- Python 3.7+
- pytest 7.4.0+
- HeliosDB-Lite binary (cargo build --release)
- See requirements.txt for full list

## License

Apache 2.0
