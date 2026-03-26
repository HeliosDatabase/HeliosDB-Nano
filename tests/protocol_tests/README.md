# HeliosDB Nano Protocol Integration Tests

This directory contains comprehensive protocol integration tests for HeliosDB Nano, validating both PostgreSQL and Oracle wire protocol implementations.

## Overview

The test suite validates:
- **PostgreSQL Protocol**: Full CRUD operations, session tracking, and pg_stat_activity view
- **Oracle Protocol**: Full CRUD operations, Oracle-specific SQL features, and v$session view
- **Session Management**: Active session tracking through system tables and views

## Prerequisites

### Required Software

- Python 3.8 or higher
- HeliosDB Nano server running with protocol support
- PostgreSQL client libraries (for psycopg2)
- Oracle Instant Client (for oracledb)

### Python Dependencies

Install via requirements.txt:

```bash
pip install -r requirements.txt
```

Dependencies include:
- `psycopg2-binary>=2.9.9` - PostgreSQL adapter
- `oracledb>=2.0.0` - Oracle Database adapter
- `pytest>=7.4.0` - Testing framework
- `colorama>=0.4.6` - Terminal colors

## Running Tests

### Quick Start

Run all protocol tests:

```bash
./run_tests.sh
```

### Individual Tests

Run PostgreSQL protocol tests only:

```bash
python3 test_postgres.py
```

Run Oracle protocol tests only:

```bash
python3 test_oracle.py
```

## Test Coverage

### PostgreSQL Protocol Tests

**File**: `test_postgres.py`

Tests:
1. Connection establishment to PostgreSQL port (5432)
2. CREATE TABLE with PostgreSQL types (SERIAL, TEXT, INTEGER, TIMESTAMP)
3. INSERT with parameterized queries and RETURNING clause
4. SELECT with WHERE clauses and ORDER BY
5. UPDATE with parameterized queries
6. DELETE with WHERE conditions
7. Session tracking via `helios_sessions` table
8. PostgreSQL compatibility view `pg_stat_activity`
9. Transaction management (COMMIT/ROLLBACK)
10. Cleanup operations

**Key Features Tested**:
- PostgreSQL wire protocol
- Parameterized queries with `%s` placeholders
- SERIAL primary keys
- RETURNING clause
- Session state tracking
- pg_stat_activity view compatibility

### Oracle Protocol Tests

**File**: `test_oracle.py`

Tests:
1. Connection establishment to Oracle port (1521)
2. CREATE TABLE with Oracle types (NUMBER, VARCHAR2, DATE)
3. INSERT with Oracle-specific syntax
4. SELECT with Oracle functions (SYSDATE, USER, UPPER, TO_CHAR, DECODE, NVL)
5. UPDATE with Oracle functions (NVL)
6. DELETE with Oracle WHERE conditions
7. Session tracking via `v$session` view
8. ROWNUM limiting
9. GROUP BY with HAVING clause
10. Aggregate functions (COUNT, AVG)
11. Cleanup operations

**Key Features Tested**:
- Oracle wire protocol (TDS)
- Oracle SQL syntax and functions
- DUAL table queries
- Oracle data types (NUMBER, VARCHAR2, DATE)
- DECODE and NVL functions
- ROWNUM pseudo-column
- v$session view compatibility

## System Tables and Views

### helios_sessions Table

Core system table for session tracking:

```sql
CREATE TABLE helios_sessions (
    session_id INT8 PRIMARY KEY,
    protocol TEXT NOT NULL,
    username TEXT NOT NULL,
    client_address TEXT NOT NULL,
    client_port INT4 NOT NULL,
    connect_time TIMESTAMP NOT NULL,
    last_activity TIMESTAMP NOT NULL,
    current_query TEXT,
    state TEXT NOT NULL  -- 'active', 'idle', 'idle_in_transaction'
);
```

### pg_stat_activity View

PostgreSQL compatibility view:

```sql
CREATE VIEW pg_stat_activity AS
SELECT
    session_id AS pid,
    username AS usename,
    'heliosdb' AS datname,
    client_address AS client_addr,
    client_port,
    connect_time AS backend_start,
    last_activity AS state_change,
    current_query AS query,
    state,
    protocol AS application_name
FROM helios_sessions
WHERE protocol = 'PostgreSQL';
```

### v$session View

Oracle compatibility view:

```sql
CREATE VIEW v$session AS
SELECT
    session_id AS sid,
    session_id AS serial#,
    username,
    state AS status,
    client_address AS machine,
    protocol AS program,
    connect_time AS logon_time,
    last_activity AS last_call_et,
    current_query AS sql_text,
    CASE
        WHEN state = 'active' THEN 'ACTIVE'
        WHEN state = 'idle' THEN 'INACTIVE'
        ELSE 'SNIPED'
    END AS status
FROM helios_sessions
WHERE protocol = 'Oracle';
```

## Test Output

### Successful Test Output

```
==========================================
HeliosDB Nano Protocol Integration Tests
==========================================

Installing Python dependencies...

==========================================
Running: PostgreSQL Protocol Test
==========================================

============================================================
PostgreSQL Protocol Test Suite
============================================================

[1/7] Connecting to HeliosDB via PostgreSQL protocol...
✓ Connection established

[2/7] Creating test table...
✓ Table 'test_users' created

[3/7] Inserting test data...
✓ Inserted 3 rows (IDs: 1, 2, 3)

[4/7] Reading test data...
✓ Selected 3 rows:
  ID=1, Name=Alice Johnson, Email=alice@example.com, Age=30
  ID=2, Name=Bob Smith, Email=bob@example.com, Age=25
  ID=3, Name=Charlie Davis, Email=charlie@example.com, Age=35

[5/7] Updating test data...
✓ Updated 1 row(s)
  Verified: Alice's age is now 31, email is alice.johnson@example.com

[6/7] Deleting test data...
✓ Deleted 1 row(s)
  Remaining rows: 2

[7/7] Checking active sessions...
✓ Found 1 active session(s):
  Session 1: PostgreSQL - test_user (active)
    Query: SELECT session_id, protocol, username, state...

[BONUS] Testing pg_stat_activity view...
✓ pg_stat_activity returned 1 row(s)
  PID=1, User=test_user, DB=heliosdb, State=active

[CLEANUP] Dropping test table...
✓ Test table dropped

============================================================
✅ PostgreSQL protocol test PASSED
============================================================

✅ PostgreSQL Protocol Test PASSED

... [Oracle test output] ...

==========================================
Test Summary
==========================================

Tests Passed: 2
Tests Failed: 0

✅ All protocol tests completed successfully!
```

## Troubleshooting

### Connection Refused

**Error**: `psycopg2.OperationalError: could not connect to server`

**Solutions**:
1. Ensure HeliosDB server is running
2. Check that PostgreSQL protocol is enabled on port 5432
3. Verify firewall settings allow connections

### Authentication Failed

**Error**: `authentication failed for user "test_user"`

**Solutions**:
1. Verify user credentials in test scripts
2. Check HeliosDB authentication configuration
3. Ensure user has proper permissions

### Missing Dependencies

**Error**: `ModuleNotFoundError: No module named 'psycopg2'`

**Solutions**:
1. Run `pip install -r requirements.txt`
2. Ensure virtual environment is activated
3. Check Python version compatibility

### Oracle Client Not Found

**Error**: `DPI-1047: Cannot locate an Oracle Client library`

**Solutions**:
1. Install Oracle Instant Client
2. Set `LD_LIBRARY_PATH` or `DYLD_LIBRARY_PATH`
3. Configure `oracledb.init_oracle_client()`

## Development

### Adding New Tests

To add new protocol tests:

1. Create a new test function in the appropriate test file
2. Follow the existing test structure and naming conventions
3. Add descriptive output for each test step
4. Include proper error handling and cleanup
5. Update this README with new test coverage

### Test Structure

Each test should follow this pattern:

```python
def test_feature():
    """Test description"""
    try:
        # Setup
        conn = connect_to_database()
        cursor = conn.cursor()

        # Test operations with descriptive output
        print("[1/N] Doing something...")
        cursor.execute("SQL QUERY")
        print("✓ Success message")

        # Cleanup
        conn.close()
        return True

    except Exception as e:
        print(f"❌ Error: {e}")
        return False
```

## Integration with CI/CD

These tests can be integrated into CI/CD pipelines:

```yaml
# Example GitHub Actions workflow
- name: Run Protocol Tests
  run: |
    cd tests/protocol_tests
    ./run_tests.sh
```

## Related Documentation

- [Protocol Integration Guide](../../docs/guides/PROTOCOL_INTEGRATION.md)
- [Session Management](../../docs/architecture/SESSION_MANAGEMENT.md)
- [PostgreSQL Compatibility](../../docs/guides/POSTGRESQL_COMPATIBILITY.md)
- [Oracle Compatibility](../../docs/guides/ORACLE_COMPATIBILITY.md)

## License

Copyright (C) 2025 HeliosDB Project
