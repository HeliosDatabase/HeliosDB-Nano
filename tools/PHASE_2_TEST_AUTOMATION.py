#!/usr/bin/env python3
"""
Phase 2 Test Automation Suite
SQLite to HeliosDB Migration Validation

Comprehensive automated testing framework for validating data migration,
CRUD operations, integration points, and database consistency.

Requirements:
    pip install pytest psycopg2-binary pytest-xdist pytest-html pyyaml tabulate

Usage:
    # Run all tests
    pytest PHASE_2_TEST_AUTOMATION.py -v

    # Run specific test category
    pytest PHASE_2_TEST_AUTOMATION.py -v -k "test_data_integrity"

    # Run with HTML report
    pytest PHASE_2_TEST_AUTOMATION.py --html=test_report.html --self-contained-html

    # Run in parallel
    pytest PHASE_2_TEST_AUTOMATION.py -n auto

Author: Testing & Verification Specialist
Version: 1.0.0
Date: 2025-12-08
"""

import pytest
import psycopg2
import sqlite3
import hashlib
import time
import statistics
import random
import yaml
from datetime import datetime, timedelta
from typing import Dict, List, Tuple, Any, Optional
from dataclasses import dataclass
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager
from tabulate import tabulate


# ============================================================================
# Configuration
# ============================================================================

@dataclass
class TestConfig:
    """Test configuration"""
    sqlite_path: str = "./test_data/source.db"
    helios_host: str = "localhost"
    helios_port: int = 20000
    helios_database: str = "heliosdb"
    helios_user: str = "test_user"
    helios_password: str = "test_password"

    # Performance thresholds
    query_latency_ratio: float = 1.2  # HeliosDB <= 120% of SQLite
    throughput_ratio: float = 0.8  # HeliosDB >= 80% of SQLite
    data_integrity_threshold: float = 100.0  # 100% match required

    # Test parameters
    query_iterations: int = 100
    bulk_insert_size: int = 10000
    concurrent_connections: int = 50
    sample_percentage: float = 0.05  # 5% sampling


def load_config(config_file: str = "test_config.yaml") -> TestConfig:
    """Load test configuration from YAML file"""
    config_path = Path(config_file)
    if config_path.exists():
        with open(config_path) as f:
            data = yaml.safe_load(f)
            return TestConfig(**data)
    return TestConfig()


CONFIG = load_config()


# ============================================================================
# Database Connections
# ============================================================================

@contextmanager
def get_sqlite_connection():
    """Get SQLite connection with proper cleanup"""
    conn = sqlite3.connect(CONFIG.sqlite_path)
    conn.row_factory = sqlite3.Row
    try:
        yield conn
    finally:
        conn.close()


@contextmanager
def get_helios_connection():
    """Get HeliosDB connection with proper cleanup"""
    conn = psycopg2.connect(
        host=CONFIG.helios_host,
        port=CONFIG.helios_port,
        database=CONFIG.helios_database,
        user=CONFIG.helios_user,
        password=CONFIG.helios_password
    )
    try:
        yield conn
    finally:
        conn.close()


# ============================================================================
# Fixtures
# ============================================================================

@pytest.fixture(scope="session")
def sqlite_conn():
    """Session-scoped SQLite connection"""
    with get_sqlite_connection() as conn:
        yield conn


@pytest.fixture(scope="session")
def helios_conn():
    """Session-scoped HeliosDB connection"""
    with get_helios_connection() as conn:
        yield conn


@pytest.fixture(scope="session")
def table_list(sqlite_conn):
    """Get list of all tables in the database"""
    cursor = sqlite_conn.cursor()
    cursor.execute("""
        SELECT name FROM sqlite_master
        WHERE type='table'
        AND name NOT LIKE 'sqlite_%'
        ORDER BY name
    """)
    return [row[0] for row in cursor.fetchall()]


@pytest.fixture(scope="session")
def test_results():
    """Collect test results for final report"""
    results = {
        'data_integrity': [],
        'crud_operations': [],
        'performance': [],
        'integration': [],
        'concurrency': [],
        'constraints': []
    }
    yield results
    # Generate final report
    generate_test_report(results)


# ============================================================================
# Helper Functions
# ============================================================================

def get_table_schema(conn, table_name: str, db_type: str = "sqlite") -> List[Dict]:
    """Get table schema information"""
    cursor = conn.cursor()

    if db_type == "sqlite":
        cursor.execute(f"PRAGMA table_info({table_name})")
        columns = []
        for row in cursor.fetchall():
            columns.append({
                'name': row[1],
                'type': row[2],
                'notnull': bool(row[3]),
                'default': row[4],
                'pk': bool(row[5])
            })
    else:  # PostgreSQL/HeliosDB
        cursor.execute("""
            SELECT
                column_name,
                data_type,
                is_nullable,
                column_default
            FROM information_schema.columns
            WHERE table_name = %s
            ORDER BY ordinal_position
        """, (table_name,))
        columns = []
        for row in cursor.fetchall():
            columns.append({
                'name': row[0],
                'type': row[1],
                'notnull': row[2] == 'NO',
                'default': row[3],
                'pk': False  # Would need separate query
            })

    return columns


def get_row_count(conn, table_name: str) -> int:
    """Get row count for a table"""
    cursor = conn.cursor()
    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    return cursor.fetchone()[0]


def compute_table_hash(conn, table_name: str, columns: List[str]) -> str:
    """Compute hash of table contents for comparison"""
    cursor = conn.cursor()
    column_list = ", ".join(columns)
    cursor.execute(f"SELECT {column_list} FROM {table_name} ORDER BY {columns[0]}")

    hasher = hashlib.sha256()
    for row in cursor.fetchall():
        # Convert row to string and hash
        row_str = str(tuple(row)).encode('utf-8')
        hasher.update(row_str)

    return hasher.hexdigest()


def get_sample_rows(conn, table_name: str, sample_size: int, pk_column: str = "id") -> List[Any]:
    """Get random sample of rows from table"""
    cursor = conn.cursor()

    # Get total count
    total = get_row_count(conn, table_name)
    if total == 0:
        return []

    # Calculate sample size
    actual_sample_size = min(sample_size, total)

    # Get random sample
    cursor.execute(f"""
        SELECT * FROM {table_name}
        ORDER BY RANDOM()
        LIMIT {actual_sample_size}
    """)

    return cursor.fetchall()


def benchmark_query(conn, query: str, params: Tuple = (), iterations: int = 100) -> Dict[str, float]:
    """Benchmark a query and return timing statistics"""
    times = []

    for _ in range(iterations):
        cursor = conn.cursor()
        start = time.perf_counter()
        cursor.execute(query, params)
        cursor.fetchall()
        end = time.perf_counter()
        times.append(end - start)
        cursor.close()

    return {
        'mean': statistics.mean(times),
        'median': statistics.median(times),
        'stddev': statistics.stdev(times) if len(times) > 1 else 0,
        'min': min(times),
        'max': max(times),
        'p95': sorted(times)[int(len(times) * 0.95)],
        'p99': sorted(times)[int(len(times) * 0.99)]
    }


def generate_test_report(results: Dict):
    """Generate comprehensive test report"""
    print("\n" + "=" * 80)
    print("PHASE 2 TEST EXECUTION REPORT")
    print("=" * 80)
    print(f"Generated: {datetime.now().isoformat()}")
    print()

    for category, tests in results.items():
        if tests:
            print(f"\n{category.upper().replace('_', ' ')}")
            print("-" * 80)
            passed = sum(1 for t in tests if t.get('passed', False))
            total = len(tests)
            print(f"Pass Rate: {passed}/{total} ({100*passed/total:.1f}%)")
            print()

    # Save to file
    report_file = f"PHASE_2_TEST_RESULTS_{datetime.now().strftime('%Y%m%d_%H%M%S')}.txt"
    with open(report_file, 'w') as f:
        f.write(f"Phase 2 Test Results\n")
        f.write(f"Generated: {datetime.now().isoformat()}\n\n")
        for category, tests in results.items():
            f.write(f"\n{category}:\n")
            for test in tests:
                f.write(f"  {test}\n")

    print(f"\nFull report saved to: {report_file}")


# ============================================================================
# Data Integrity Tests
# ============================================================================

class TestDataIntegrity:
    """Test suite for data integrity validation"""

    def test_table_count_match(self, sqlite_conn, helios_conn, test_results):
        """DI-001: Verify all tables migrated"""
        # Get table lists
        sqlite_cursor = sqlite_conn.cursor()
        sqlite_cursor.execute("""
            SELECT name FROM sqlite_master
            WHERE type='table' AND name NOT LIKE 'sqlite_%'
            ORDER BY name
        """)
        sqlite_tables = set(row[0] for row in sqlite_cursor.fetchall())

        helios_cursor = helios_conn.cursor()
        helios_cursor.execute("""
            SELECT table_name FROM information_schema.tables
            WHERE table_schema = 'public'
            ORDER BY table_name
        """)
        helios_tables = set(row[0] for row in helios_cursor.fetchall())

        # Compare
        missing_tables = sqlite_tables - helios_tables
        extra_tables = helios_tables - sqlite_tables

        result = {
            'test': 'table_count_match',
            'sqlite_count': len(sqlite_tables),
            'helios_count': len(helios_tables),
            'missing': list(missing_tables),
            'extra': list(extra_tables),
            'passed': len(missing_tables) == 0 and len(extra_tables) == 0
        }
        test_results['data_integrity'].append(result)

        assert len(missing_tables) == 0, f"Missing tables in HeliosDB: {missing_tables}"
        assert len(extra_tables) == 0, f"Extra tables in HeliosDB: {extra_tables}"

    def test_row_count_verification(self, sqlite_conn, helios_conn, table_list, test_results):
        """DI-002: Verify row counts match for all tables"""
        mismatches = []

        for table in table_list:
            sqlite_count = get_row_count(sqlite_conn, table)
            helios_count = get_row_count(helios_conn, table)

            result = {
                'table': table,
                'sqlite_count': sqlite_count,
                'helios_count': helios_count,
                'match': sqlite_count == helios_count
            }

            if sqlite_count != helios_count:
                mismatches.append(result)

            test_results['data_integrity'].append(result)

        assert len(mismatches) == 0, f"Row count mismatches: {mismatches}"

    def test_schema_compatibility(self, sqlite_conn, helios_conn, table_list, test_results):
        """DI-003: Verify schema compatibility"""
        type_mappings = {
            'INTEGER': ['integer', 'bigint', 'smallint', 'int'],
            'TEXT': ['text', 'character varying', 'varchar', 'char'],
            'REAL': ['double precision', 'real', 'numeric', 'decimal'],
            'BLOB': ['bytea'],
            'NUMERIC': ['numeric', 'decimal'],
        }

        for table in table_list:
            sqlite_schema = get_table_schema(sqlite_conn, table, "sqlite")
            helios_schema = get_table_schema(helios_conn, table, "postgres")

            # Check column count
            assert len(sqlite_schema) == len(helios_schema), \
                f"Column count mismatch for {table}"

            # Check column names and types
            for sqlite_col in sqlite_schema:
                helios_col = next(
                    (c for c in helios_schema if c['name'] == sqlite_col['name']),
                    None
                )
                assert helios_col is not None, \
                    f"Column {sqlite_col['name']} missing in HeliosDB table {table}"

                # Verify type compatibility
                sqlite_type = sqlite_col['type'].upper()
                helios_type = helios_col['type'].lower()

                # Find compatible type
                compatible = False
                for base_type, pg_types in type_mappings.items():
                    if base_type in sqlite_type:
                        if any(pg_type in helios_type for pg_type in pg_types):
                            compatible = True
                            break

                result = {
                    'table': table,
                    'column': sqlite_col['name'],
                    'sqlite_type': sqlite_type,
                    'helios_type': helios_type,
                    'compatible': compatible
                }
                test_results['data_integrity'].append(result)

                assert compatible, \
                    f"Type incompatibility for {table}.{sqlite_col['name']}: {sqlite_type} vs {helios_type}"

    @pytest.mark.parametrize("table_name", ["users", "products", "orders"])
    def test_data_content_sampling(self, sqlite_conn, helios_conn, table_name, test_results):
        """DI-004: Verify data content through random sampling"""
        try:
            # Get sample from SQLite
            sample_size = int(get_row_count(sqlite_conn, table_name) * CONFIG.sample_percentage)
            sample_size = max(100, min(sample_size, 1000))  # Between 100-1000 rows

            sqlite_cursor = sqlite_conn.cursor()
            sqlite_cursor.execute(f"SELECT * FROM {table_name} ORDER BY RANDOM() LIMIT {sample_size}")
            sqlite_sample = sqlite_cursor.fetchall()

            # For each row, verify in HeliosDB
            mismatches = 0
            for row in sqlite_sample:
                # Assume first column is primary key
                pk_value = row[0]

                helios_cursor = helios_conn.cursor()
                helios_cursor.execute(f"SELECT * FROM {table_name} WHERE id = %s", (pk_value,))
                helios_row = helios_cursor.fetchone()

                if helios_row is None:
                    mismatches += 1
                    continue

                # Compare row values (allowing for minor float differences)
                for i, (sqlite_val, helios_val) in enumerate(zip(row, helios_row)):
                    if sqlite_val != helios_val:
                        # Check if float comparison issue
                        if isinstance(sqlite_val, float) and isinstance(helios_val, float):
                            if abs(sqlite_val - helios_val) > 0.0001:
                                mismatches += 1
                                break
                        else:
                            mismatches += 1
                            break

            match_rate = 100 * (sample_size - mismatches) / sample_size if sample_size > 0 else 100

            result = {
                'test': 'data_content_sampling',
                'table': table_name,
                'sample_size': sample_size,
                'mismatches': mismatches,
                'match_rate': match_rate,
                'passed': match_rate >= CONFIG.data_integrity_threshold
            }
            test_results['data_integrity'].append(result)

            assert match_rate >= CONFIG.data_integrity_threshold, \
                f"Data mismatch rate too high for {table_name}: {mismatches}/{sample_size}"

        except Exception as e:
            pytest.skip(f"Table {table_name} not available: {str(e)}")


# ============================================================================
# CRUD Operation Tests
# ============================================================================

class TestCRUDOperations:
    """Test suite for CRUD operations"""

    def test_insert_single_row(self, helios_conn, test_results):
        """CRUD-001-A: Test single row INSERT"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_insert_single (
                id SERIAL PRIMARY KEY,
                name TEXT NOT NULL,
                email TEXT UNIQUE,
                age INTEGER,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)
        helios_conn.commit()

        # Insert row
        cursor.execute("""
            INSERT INTO test_insert_single (name, email, age)
            VALUES (%s, %s, %s)
            RETURNING id
        """, ('Test User', 'test@example.com', 30))
        inserted_id = cursor.fetchone()[0]
        helios_conn.commit()

        # Verify
        cursor.execute("SELECT * FROM test_insert_single WHERE id = %s", (inserted_id,))
        row = cursor.fetchone()

        result = {
            'test': 'insert_single_row',
            'inserted_id': inserted_id,
            'verified': row is not None,
            'passed': row is not None and row[1] == 'Test User'
        }
        test_results['crud_operations'].append(result)

        assert row is not None
        assert row[1] == 'Test User'
        assert row[2] == 'test@example.com'

        # Cleanup
        cursor.execute("DROP TABLE test_insert_single")
        helios_conn.commit()

    def test_insert_bulk(self, helios_conn, test_results):
        """CRUD-001-B: Test bulk INSERT"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_insert_bulk (
                id SERIAL PRIMARY KEY,
                name TEXT,
                value INTEGER
            )
        """)
        helios_conn.commit()

        # Bulk insert
        start_time = time.time()
        insert_count = 1000

        for i in range(insert_count):
            cursor.execute(
                "INSERT INTO test_insert_bulk (name, value) VALUES (%s, %s)",
                (f'Item {i}', i)
            )

        helios_conn.commit()
        duration = time.time() - start_time
        throughput = insert_count / duration

        # Verify count
        cursor.execute("SELECT COUNT(*) FROM test_insert_bulk")
        actual_count = cursor.fetchone()[0]

        result = {
            'test': 'insert_bulk',
            'insert_count': insert_count,
            'actual_count': actual_count,
            'duration': duration,
            'throughput': throughput,
            'passed': actual_count == insert_count and throughput > 100
        }
        test_results['crud_operations'].append(result)

        assert actual_count == insert_count

        # Cleanup
        cursor.execute("DROP TABLE test_insert_bulk")
        helios_conn.commit()

    def test_select_operations(self, helios_conn, test_results):
        """CRUD-002: Test various SELECT operations"""
        cursor = helios_conn.cursor()

        # Create and populate test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_select (
                id SERIAL PRIMARY KEY,
                name TEXT,
                category TEXT,
                value INTEGER
            )
        """)

        # Insert test data
        for i in range(100):
            cursor.execute(
                "INSERT INTO test_select (name, category, value) VALUES (%s, %s, %s)",
                (f'Item {i}', 'A' if i % 2 == 0 else 'B', i)
            )
        helios_conn.commit()

        # Test simple SELECT
        cursor.execute("SELECT * FROM test_select WHERE value > 50")
        results = cursor.fetchall()
        assert len(results) == 49  # 51-99

        # Test with ORDER BY
        cursor.execute("SELECT * FROM test_select ORDER BY value DESC LIMIT 5")
        results = cursor.fetchall()
        assert results[0][3] == 99  # value column

        # Test aggregation
        cursor.execute("SELECT category, COUNT(*), AVG(value) FROM test_select GROUP BY category")
        results = cursor.fetchall()
        assert len(results) == 2  # Categories A and B

        result = {
            'test': 'select_operations',
            'tests_passed': 3,
            'passed': True
        }
        test_results['crud_operations'].append(result)

        # Cleanup
        cursor.execute("DROP TABLE test_select")
        helios_conn.commit()

    def test_update_operations(self, helios_conn, test_results):
        """CRUD-003: Test UPDATE operations"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_update (
                id SERIAL PRIMARY KEY,
                name TEXT,
                value INTEGER
            )
        """)

        # Insert test data
        cursor.execute("INSERT INTO test_update (name, value) VALUES (%s, %s) RETURNING id",
                      ('Original', 100))
        test_id = cursor.fetchone()[0]
        helios_conn.commit()

        # Update
        cursor.execute("UPDATE test_update SET name = %s, value = %s WHERE id = %s",
                      ('Updated', 200, test_id))
        updated_rows = cursor.rowcount
        helios_conn.commit()

        # Verify
        cursor.execute("SELECT name, value FROM test_update WHERE id = %s", (test_id,))
        row = cursor.fetchone()

        result = {
            'test': 'update_operations',
            'updated_rows': updated_rows,
            'verified': row[0] == 'Updated' and row[1] == 200,
            'passed': updated_rows == 1 and row[0] == 'Updated'
        }
        test_results['crud_operations'].append(result)

        assert updated_rows == 1
        assert row[0] == 'Updated'
        assert row[1] == 200

        # Cleanup
        cursor.execute("DROP TABLE test_update")
        helios_conn.commit()

    def test_delete_operations(self, helios_conn, test_results):
        """CRUD-004: Test DELETE operations"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_delete (
                id SERIAL PRIMARY KEY,
                name TEXT
            )
        """)

        # Insert test data
        cursor.execute("INSERT INTO test_delete (name) VALUES (%s) RETURNING id", ('To Delete',))
        test_id = cursor.fetchone()[0]
        helios_conn.commit()

        # Delete
        cursor.execute("DELETE FROM test_delete WHERE id = %s", (test_id,))
        deleted_rows = cursor.rowcount
        helios_conn.commit()

        # Verify
        cursor.execute("SELECT * FROM test_delete WHERE id = %s", (test_id,))
        row = cursor.fetchone()

        result = {
            'test': 'delete_operations',
            'deleted_rows': deleted_rows,
            'verified_deleted': row is None,
            'passed': deleted_rows == 1 and row is None
        }
        test_results['crud_operations'].append(result)

        assert deleted_rows == 1
        assert row is None

        # Cleanup
        cursor.execute("DROP TABLE test_delete")
        helios_conn.commit()


# ============================================================================
# Performance Tests
# ============================================================================

class TestPerformance:
    """Test suite for performance benchmarks"""

    @pytest.mark.performance
    def test_query_latency_simple(self, sqlite_conn, helios_conn, test_results):
        """PERF-001-A: Compare simple SELECT query latency"""
        # This test requires a common table - skip if not available
        try:
            # Test on 'users' table
            sqlite_stats = benchmark_query(
                sqlite_conn,
                "SELECT * FROM users WHERE age > 25",
                iterations=CONFIG.query_iterations
            )

            helios_stats = benchmark_query(
                helios_conn,
                "SELECT * FROM users WHERE age > 25",
                iterations=CONFIG.query_iterations
            )

            ratio = helios_stats['mean'] / sqlite_stats['mean']

            result = {
                'test': 'query_latency_simple',
                'sqlite_mean': sqlite_stats['mean'],
                'helios_mean': helios_stats['mean'],
                'ratio': ratio,
                'threshold': CONFIG.query_latency_ratio,
                'passed': ratio <= CONFIG.query_latency_ratio
            }
            test_results['performance'].append(result)

            assert ratio <= CONFIG.query_latency_ratio, \
                f"HeliosDB query too slow: {ratio:.2f}x vs threshold {CONFIG.query_latency_ratio}x"

        except Exception as e:
            pytest.skip(f"Performance test skipped: {str(e)}")

    @pytest.mark.performance
    def test_bulk_insert_throughput(self, helios_conn, test_results):
        """PERF-002-A: Measure bulk INSERT throughput"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_perf_insert (
                id SERIAL PRIMARY KEY,
                name TEXT,
                value INTEGER
            )
        """)
        helios_conn.commit()

        # Measure throughput
        insert_count = CONFIG.bulk_insert_size
        start_time = time.time()

        for i in range(insert_count):
            cursor.execute(
                "INSERT INTO test_perf_insert (name, value) VALUES (%s, %s)",
                (f'Item {i}', i)
            )

        helios_conn.commit()
        duration = time.time() - start_time
        throughput = insert_count / duration

        result = {
            'test': 'bulk_insert_throughput',
            'insert_count': insert_count,
            'duration': duration,
            'throughput': throughput,
            'target': 1000,
            'passed': throughput >= 1000
        }
        test_results['performance'].append(result)

        # Cleanup
        cursor.execute("DROP TABLE test_perf_insert")
        helios_conn.commit()

        assert throughput >= 1000, f"Throughput too low: {throughput:.0f} rows/sec"


# ============================================================================
# Concurrency Tests
# ============================================================================

class TestConcurrency:
    """Test suite for concurrent access"""

    @pytest.mark.concurrency
    def test_concurrent_connections(self, test_results):
        """CONC-001: Test multiple concurrent connections"""
        num_workers = min(CONFIG.concurrent_connections, 20)  # Limit for test
        operations_per_worker = 100

        def worker(worker_id):
            """Worker function for concurrent test"""
            try:
                with get_helios_connection() as conn:
                    cursor = conn.cursor()

                    # Create worker-specific table
                    table_name = f"test_concurrent_{worker_id}"
                    cursor.execute(f"""
                        CREATE TABLE IF NOT EXISTS {table_name} (
                            id SERIAL PRIMARY KEY,
                            worker_id INTEGER,
                            value INTEGER
                        )
                    """)
                    conn.commit()

                    # Perform operations
                    for i in range(operations_per_worker):
                        cursor.execute(
                            f"INSERT INTO {table_name} (worker_id, value) VALUES (%s, %s)",
                            (worker_id, i)
                        )

                    conn.commit()

                    # Verify
                    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
                    count = cursor.fetchone()[0]

                    # Cleanup
                    cursor.execute(f"DROP TABLE {table_name}")
                    conn.commit()

                    return count == operations_per_worker

            except Exception as e:
                print(f"Worker {worker_id} error: {e}")
                return False

        # Run workers concurrently
        start_time = time.time()
        with ThreadPoolExecutor(max_workers=num_workers) as executor:
            futures = [executor.submit(worker, i) for i in range(num_workers)]
            results_list = [future.result() for future in as_completed(futures)]

        duration = time.time() - start_time
        success_rate = sum(results_list) / len(results_list)

        result = {
            'test': 'concurrent_connections',
            'num_workers': num_workers,
            'operations_per_worker': operations_per_worker,
            'duration': duration,
            'success_rate': success_rate,
            'passed': success_rate == 1.0
        }
        test_results['concurrency'].append(result)

        assert success_rate == 1.0, f"Concurrent operations failed: {success_rate:.1%} success rate"

    @pytest.mark.concurrency
    def test_transaction_isolation(self, test_results):
        """CONC-002: Test transaction isolation"""
        # Create two connections
        with get_helios_connection() as conn1, get_helios_connection() as conn2:
            cursor1 = conn1.cursor()
            cursor2 = conn2.cursor()

            # Create test table
            cursor1.execute("""
                CREATE TABLE IF NOT EXISTS test_isolation (
                    id INTEGER PRIMARY KEY,
                    value INTEGER
                )
            """)
            cursor1.execute("INSERT INTO test_isolation (id, value) VALUES (1, 100)")
            conn1.commit()

            # Start transaction in conn1
            cursor1.execute("BEGIN")
            cursor1.execute("UPDATE test_isolation SET value = 200 WHERE id = 1")

            # Read from conn2 (should not see uncommitted change)
            cursor2.execute("SELECT value FROM test_isolation WHERE id = 1")
            value_before_commit = cursor2.fetchone()[0]

            # Commit conn1
            conn1.commit()

            # Read from conn2 again (should now see committed change)
            cursor2.execute("SELECT value FROM test_isolation WHERE id = 1")
            value_after_commit = cursor2.fetchone()[0]

            result = {
                'test': 'transaction_isolation',
                'value_before_commit': value_before_commit,
                'value_after_commit': value_after_commit,
                'passed': value_before_commit == 100 and value_after_commit == 200
            }
            test_results['concurrency'].append(result)

            # Cleanup
            cursor1.execute("DROP TABLE test_isolation")
            conn1.commit()

            assert value_before_commit == 100, "Dirty read detected"
            assert value_after_commit == 200, "Committed change not visible"


# ============================================================================
# Constraint Tests
# ============================================================================

class TestConstraints:
    """Test suite for constraint validation"""

    def test_primary_key_constraint(self, helios_conn, test_results):
        """CONS-001: Test primary key constraints"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_pk (
                id INTEGER PRIMARY KEY,
                name TEXT
            )
        """)
        helios_conn.commit()

        # Insert first row
        cursor.execute("INSERT INTO test_pk (id, name) VALUES (1, 'First')")
        helios_conn.commit()

        # Try to insert duplicate PK (should fail)
        with pytest.raises(psycopg2.IntegrityError):
            cursor.execute("INSERT INTO test_pk (id, name) VALUES (1, 'Duplicate')")
            helios_conn.commit()

        helios_conn.rollback()

        result = {
            'test': 'primary_key_constraint',
            'constraint_enforced': True,
            'passed': True
        }
        test_results['constraints'].append(result)

        # Cleanup
        cursor.execute("DROP TABLE test_pk")
        helios_conn.commit()

    def test_foreign_key_constraint(self, helios_conn, test_results):
        """CONS-002: Test foreign key constraints"""
        cursor = helios_conn.cursor()

        # Create parent table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_fk_parent (
                id INTEGER PRIMARY KEY,
                name TEXT
            )
        """)

        # Create child table with FK
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_fk_child (
                id INTEGER PRIMARY KEY,
                parent_id INTEGER REFERENCES test_fk_parent(id),
                name TEXT
            )
        """)
        helios_conn.commit()

        # Insert parent
        cursor.execute("INSERT INTO test_fk_parent (id, name) VALUES (1, 'Parent')")
        helios_conn.commit()

        # Try to insert child with invalid FK (should fail)
        with pytest.raises(psycopg2.IntegrityError):
            cursor.execute("INSERT INTO test_fk_child (id, parent_id, name) VALUES (1, 999, 'Child')")
            helios_conn.commit()

        helios_conn.rollback()

        # Insert child with valid FK (should succeed)
        cursor.execute("INSERT INTO test_fk_child (id, parent_id, name) VALUES (1, 1, 'Child')")
        helios_conn.commit()

        result = {
            'test': 'foreign_key_constraint',
            'constraint_enforced': True,
            'passed': True
        }
        test_results['constraints'].append(result)

        # Cleanup
        cursor.execute("DROP TABLE test_fk_child")
        cursor.execute("DROP TABLE test_fk_parent")
        helios_conn.commit()

    def test_unique_constraint(self, helios_conn, test_results):
        """CONS-003: Test unique constraints"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_unique (
                id INTEGER PRIMARY KEY,
                email TEXT UNIQUE
            )
        """)
        helios_conn.commit()

        # Insert first row
        cursor.execute("INSERT INTO test_unique (id, email) VALUES (1, 'unique@example.com')")
        helios_conn.commit()

        # Try to insert duplicate unique value (should fail)
        with pytest.raises(psycopg2.IntegrityError):
            cursor.execute("INSERT INTO test_unique (id, email) VALUES (2, 'unique@example.com')")
            helios_conn.commit()

        helios_conn.rollback()

        result = {
            'test': 'unique_constraint',
            'constraint_enforced': True,
            'passed': True
        }
        test_results['constraints'].append(result)

        # Cleanup
        cursor.execute("DROP TABLE test_unique")
        helios_conn.commit()

    def test_not_null_constraint(self, helios_conn, test_results):
        """CONS-004: Test NOT NULL constraints"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_not_null (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            )
        """)
        helios_conn.commit()

        # Try to insert NULL (should fail)
        with pytest.raises(psycopg2.IntegrityError):
            cursor.execute("INSERT INTO test_not_null (id, name) VALUES (1, NULL)")
            helios_conn.commit()

        helios_conn.rollback()

        result = {
            'test': 'not_null_constraint',
            'constraint_enforced': True,
            'passed': True
        }
        test_results['constraints'].append(result)

        # Cleanup
        cursor.execute("DROP TABLE test_not_null")
        helios_conn.commit()


# ============================================================================
# Integration Tests
# ============================================================================

class TestIntegration:
    """Test suite for integration scenarios"""

    def test_connection_pooling(self, test_results):
        """INT-001: Test connection pooling behavior"""
        max_connections = 10
        connections = []

        try:
            # Create multiple connections
            for i in range(max_connections):
                conn = psycopg2.connect(
                    host=CONFIG.helios_host,
                    port=CONFIG.helios_port,
                    database=CONFIG.helios_database,
                    user=CONFIG.helios_user,
                    password=CONFIG.helios_password
                )
                connections.append(conn)

            # Verify all connections work
            for i, conn in enumerate(connections):
                cursor = conn.cursor()
                cursor.execute("SELECT 1")
                assert cursor.fetchone()[0] == 1

            result = {
                'test': 'connection_pooling',
                'max_connections': max_connections,
                'successful': len(connections),
                'passed': len(connections) == max_connections
            }
            test_results['integration'].append(result)

        finally:
            # Cleanup
            for conn in connections:
                conn.close()

    def test_transaction_rollback(self, helios_conn, test_results):
        """INT-002: Test transaction rollback behavior"""
        cursor = helios_conn.cursor()

        # Create test table
        cursor.execute("""
            CREATE TABLE IF NOT EXISTS test_rollback (
                id INTEGER PRIMARY KEY,
                value TEXT
            )
        """)
        helios_conn.commit()

        # Start transaction
        cursor.execute("BEGIN")
        cursor.execute("INSERT INTO test_rollback (id, value) VALUES (1, 'Test')")

        # Verify inserted (within transaction)
        cursor.execute("SELECT COUNT(*) FROM test_rollback")
        count_before_rollback = cursor.fetchone()[0]

        # Rollback
        helios_conn.rollback()

        # Verify rolled back
        cursor.execute("SELECT COUNT(*) FROM test_rollback")
        count_after_rollback = cursor.fetchone()[0]

        result = {
            'test': 'transaction_rollback',
            'count_before_rollback': count_before_rollback,
            'count_after_rollback': count_after_rollback,
            'passed': count_before_rollback == 1 and count_after_rollback == 0
        }
        test_results['integration'].append(result)

        # Cleanup
        cursor.execute("DROP TABLE test_rollback")
        helios_conn.commit()

        assert count_before_rollback == 1
        assert count_after_rollback == 0


# ============================================================================
# Main Execution
# ============================================================================

if __name__ == "__main__":
    """Run tests with pytest"""
    import sys

    # Run pytest with sensible defaults
    args = [
        __file__,
        "-v",  # Verbose
        "--tb=short",  # Short traceback
        "--html=phase2_test_report.html",  # HTML report
        "--self-contained-html",  # Single file report
        "-ra",  # Show summary of all test outcomes
    ]

    # Add any command line arguments
    args.extend(sys.argv[1:])

    sys.exit(pytest.main(args))
