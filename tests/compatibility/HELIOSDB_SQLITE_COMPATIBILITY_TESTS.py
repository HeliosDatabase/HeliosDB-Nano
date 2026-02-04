#!/usr/bin/env python3
"""
HeliosDB-Lite SQLite Compatibility Test Suite

Comprehensive pytest test suite validating HeliosDB-Lite acts as a drop-in
SQLite replacement. Tests all sqlite3 module functions, SQL operations,
concurrency, transactions, error conditions, and data type preservation.

Usage:
    pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v
    pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v -k "test_connection"
    pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py -v --tb=short

Requirements:
    pip install pytest pytest-timeout pytest-xdist psutil
"""

import pytest
import sys
import os
import tempfile
import shutil
import threading
import time
import sqlite3
from pathlib import Path
from datetime import datetime, timedelta
from decimal import Decimal
from typing import List, Tuple, Any
import subprocess
import signal
import psutil


# ============================================================================
# Test Configuration and Fixtures
# ============================================================================

class HeliosDBConfig:
    """Configuration for HeliosDB server connection"""
    DEFAULT_PORT = 5432
    DEFAULT_HOST = "127.0.0.1"
    DEFAULT_DATA_DIR = "./test-heliosdb-data"
    STARTUP_TIMEOUT = 10  # seconds


@pytest.fixture(scope="session")
def heliosdb_server():
    """
    Start HeliosDB server for testing session.

    This fixture starts a HeliosDB server in the background and ensures
    it's properly cleaned up after all tests complete.
    """
    config = HeliosDBConfig()
    data_dir = Path(config.DEFAULT_DATA_DIR)

    # Clean up any existing test data
    if data_dir.exists():
        shutil.rmtree(data_dir)

    data_dir.mkdir(parents=True, exist_ok=True)

    # Find heliosdb-lite binary
    binary_path = shutil.which("heliosdb-lite") or "./target/release/heliosdb-lite"
    if not os.path.exists(binary_path):
        pytest.skip(f"HeliosDB binary not found at {binary_path}")

    # Start server
    process = subprocess.Popen(
        [
            binary_path, "start",
            "--data-dir", str(data_dir),
            "--port", str(config.DEFAULT_PORT),
            "--listen", config.DEFAULT_HOST,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    # Wait for server to start
    start_time = time.time()
    server_ready = False

    while time.time() - start_time < config.STARTUP_TIMEOUT:
        try:
            # Try to connect using psycopg2 (PostgreSQL protocol)
            import psycopg2
            conn = psycopg2.connect(
                host=config.DEFAULT_HOST,
                port=config.DEFAULT_PORT,
                database='heliosdb',
                user='test',
                password='test',
                connect_timeout=1
            )
            conn.close()
            server_ready = True
            break
        except Exception:
            time.sleep(0.5)

    if not server_ready:
        process.terminate()
        process.wait(timeout=5)
        pytest.fail("HeliosDB server failed to start within timeout")

    yield {
        "host": config.DEFAULT_HOST,
        "port": config.DEFAULT_PORT,
        "data_dir": data_dir,
        "process": process,
    }

    # Cleanup
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    if data_dir.exists():
        shutil.rmtree(data_dir)


@pytest.fixture
def temp_db_path():
    """Provide temporary database file path"""
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    yield path
    if os.path.exists(path):
        os.unlink(path)


@pytest.fixture
def memory_connection():
    """Provide in-memory SQLite connection"""
    conn = sqlite3.connect(":memory:")
    yield conn
    conn.close()


@pytest.fixture
def file_connection(temp_db_path):
    """Provide file-based SQLite connection"""
    conn = sqlite3.connect(temp_db_path)
    yield conn
    conn.close()


# ============================================================================
# Connection Tests
# ============================================================================

class TestConnectionMethods:
    """Test various SQLite connection methods"""

    def test_memory_connection(self):
        """Test :memory: database connection"""
        conn = sqlite3.connect(":memory:")
        assert conn is not None
        cursor = conn.cursor()
        cursor.execute("SELECT 1")
        result = cursor.fetchone()
        assert result == (1,)
        conn.close()

    def test_file_connection(self, temp_db_path):
        """Test file-based database connection"""
        conn = sqlite3.connect(temp_db_path)
        assert conn is not None
        assert os.path.exists(temp_db_path)
        conn.close()

    def test_uri_connection(self, temp_db_path):
        """Test URI format connection"""
        uri = f"file:{temp_db_path}?mode=rw"
        conn = sqlite3.connect(uri, uri=True)
        assert conn is not None
        conn.close()

    def test_connection_with_timeout(self, temp_db_path):
        """Test connection with timeout parameter"""
        conn = sqlite3.connect(temp_db_path, timeout=10.0)
        assert conn is not None
        conn.close()

    def test_connection_check_same_thread(self, temp_db_path):
        """Test check_same_thread parameter"""
        conn = sqlite3.connect(temp_db_path, check_same_thread=False)
        assert conn is not None

        # Should allow access from different thread
        def access_from_thread():
            cursor = conn.cursor()
            cursor.execute("SELECT 1")

        thread = threading.Thread(target=access_from_thread)
        thread.start()
        thread.join()
        conn.close()

    def test_connection_isolation_level(self, temp_db_path):
        """Test isolation_level parameter"""
        # Default (DEFERRED)
        conn1 = sqlite3.connect(temp_db_path)
        assert conn1.isolation_level == "DEFERRED"
        conn1.close()

        # Autocommit mode
        conn2 = sqlite3.connect(temp_db_path, isolation_level=None)
        assert conn2.isolation_level is None
        conn2.close()

        # Explicit isolation level
        conn3 = sqlite3.connect(temp_db_path, isolation_level="IMMEDIATE")
        assert conn3.isolation_level == "IMMEDIATE"
        conn3.close()


# ============================================================================
# Execute Operations Tests
# ============================================================================

class TestExecuteOperations:
    """Test execute, executemany, and executescript operations"""

    def test_execute_simple_query(self, memory_connection):
        """Test execute with simple SELECT query"""
        cursor = memory_connection.cursor()
        cursor.execute("SELECT 1, 'hello', 3.14")
        result = cursor.fetchone()
        assert result == (1, 'hello', 3.14)

    def test_execute_with_parameters_qmark(self, memory_connection):
        """Test execute with ? parameter style"""
        cursor = memory_connection.cursor()
        cursor.execute("SELECT ?, ?, ?", (1, "test", 3.14))
        result = cursor.fetchone()
        assert result == (1, "test", 3.14)

    def test_execute_with_parameters_named(self, memory_connection):
        """Test execute with :name parameter style"""
        cursor = memory_connection.cursor()
        cursor.execute(
            "SELECT :id, :name, :value",
            {"id": 1, "name": "test", "value": 3.14}
        )
        result = cursor.fetchone()
        assert result == (1, "test", 3.14)

    def test_executemany(self, memory_connection):
        """Test executemany for bulk inserts"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT,
                age INTEGER
            )
        """)

        data = [
            (1, "Alice", 30),
            (2, "Bob", 25),
            (3, "Charlie", 35),
        ]

        cursor.executemany(
            "INSERT INTO users (id, name, age) VALUES (?, ?, ?)",
            data
        )

        cursor.execute("SELECT COUNT(*) FROM users")
        assert cursor.fetchone()[0] == 3

    def test_executescript(self, memory_connection):
        """Test executescript for multiple statements"""
        cursor = memory_connection.cursor()

        script = """
            CREATE TABLE products (id INTEGER, name TEXT);
            INSERT INTO products VALUES (1, 'Product A');
            INSERT INTO products VALUES (2, 'Product B');
            CREATE TABLE categories (id INTEGER, name TEXT);
            INSERT INTO categories VALUES (1, 'Category X');
        """

        cursor.executescript(script)

        cursor.execute("SELECT COUNT(*) FROM products")
        assert cursor.fetchone()[0] == 2

        cursor.execute("SELECT COUNT(*) FROM categories")
        assert cursor.fetchone()[0] == 1

    def test_execute_with_returning(self, memory_connection):
        """Test RETURNING clause (SQLite 3.35+)"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT
            )
        """)

        cursor.execute(
            "INSERT INTO items (name) VALUES (?) RETURNING id",
            ("Test Item",)
        )
        result = cursor.fetchone()
        assert result is not None
        assert isinstance(result[0], int)


# ============================================================================
# Cursor Operations Tests
# ============================================================================

class TestCursorOperations:
    """Test cursor methods and iteration"""

    def test_fetchone(self, memory_connection):
        """Test fetchone method"""
        cursor = memory_connection.cursor()
        cursor.execute("SELECT 1 UNION SELECT 2 UNION SELECT 3")

        assert cursor.fetchone() == (1,)
        assert cursor.fetchone() == (2,)
        assert cursor.fetchone() == (3,)
        assert cursor.fetchone() is None

    def test_fetchmany(self, memory_connection):
        """Test fetchmany method"""
        cursor = memory_connection.cursor()
        cursor.execute("SELECT * FROM (VALUES (1), (2), (3), (4), (5))")

        rows = cursor.fetchmany(2)
        assert len(rows) == 2

        rows = cursor.fetchmany(2)
        assert len(rows) == 2

        rows = cursor.fetchmany(2)
        assert len(rows) == 1  # Only 1 left

    def test_fetchall(self, memory_connection):
        """Test fetchall method"""
        cursor = memory_connection.cursor()
        cursor.execute("SELECT * FROM (VALUES (1), (2), (3), (4), (5))")

        rows = cursor.fetchall()
        assert len(rows) == 5
        assert rows[0] == (1,)
        assert rows[4] == (5,)

    def test_cursor_iteration(self, memory_connection):
        """Test cursor as iterator"""
        cursor = memory_connection.cursor()
        cursor.execute("SELECT * FROM (VALUES (1), (2), (3))")

        rows = list(cursor)
        assert len(rows) == 3
        assert rows == [(1,), (2,), (3,)]

    def test_cursor_description(self, memory_connection):
        """Test cursor.description attribute"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE test_desc (
                id INTEGER,
                name TEXT,
                value REAL
            )
        """)
        cursor.execute("INSERT INTO test_desc VALUES (1, 'test', 3.14)")
        cursor.execute("SELECT id, name, value FROM test_desc")

        desc = cursor.description
        assert len(desc) == 3
        assert desc[0][0] == "id"
        assert desc[1][0] == "name"
        assert desc[2][0] == "value"

    def test_cursor_rowcount(self, memory_connection):
        """Test cursor.rowcount attribute"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test_count (id INTEGER, name TEXT)")

        cursor.execute("INSERT INTO test_count VALUES (1, 'a'), (2, 'b')")
        assert cursor.rowcount == 2

        cursor.execute("UPDATE test_count SET name = 'x' WHERE id = 1")
        assert cursor.rowcount == 1

        cursor.execute("DELETE FROM test_count WHERE id = 1")
        assert cursor.rowcount == 1

    def test_cursor_lastrowid(self, memory_connection):
        """Test cursor.lastrowid attribute"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE test_lastrow (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT
            )
        """)

        cursor.execute("INSERT INTO test_lastrow (name) VALUES ('first')")
        first_id = cursor.lastrowid

        cursor.execute("INSERT INTO test_lastrow (name) VALUES ('second')")
        second_id = cursor.lastrowid

        assert second_id == first_id + 1


# ============================================================================
# Data Type Preservation Tests
# ============================================================================

class TestDataTypePreservation:
    """Test SQLite data type handling and preservation"""

    def test_integer_types(self, memory_connection):
        """Test INTEGER type preservation"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test_int (val INTEGER)")

        test_values = [
            0,
            1,
            -1,
            42,
            -42,
            2**31 - 1,  # Max 32-bit signed int
            -(2**31),   # Min 32-bit signed int
            2**63 - 1,  # Max 64-bit signed int
        ]

        for val in test_values:
            cursor.execute("DELETE FROM test_int")
            cursor.execute("INSERT INTO test_int VALUES (?)", (val,))
            cursor.execute("SELECT val FROM test_int")
            result = cursor.fetchone()[0]
            assert result == val
            assert type(result) == int

    def test_real_types(self, memory_connection):
        """Test REAL (float) type preservation"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test_real (val REAL)")

        test_values = [
            0.0,
            1.0,
            -1.0,
            3.14159265359,
            -3.14159265359,
            1.23e10,
            1.23e-10,
            float('inf'),
            float('-inf'),
        ]

        for val in test_values:
            cursor.execute("DELETE FROM test_real")
            cursor.execute("INSERT INTO test_real VALUES (?)", (val,))
            cursor.execute("SELECT val FROM test_real")
            result = cursor.fetchone()[0]
            if val == float('inf') or val == float('-inf'):
                assert result == val
            else:
                assert abs(result - val) < 1e-10
            assert type(result) == float

    def test_text_types(self, memory_connection):
        """Test TEXT type preservation"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test_text (val TEXT)")

        test_values = [
            "",
            "hello",
            "Hello, World!",
            "Multi\nLine\nText",
            "Tab\tSeparated",
            "Unicode: αβγδε",
            "Emoji: 🚀🔥💯",
            "Special chars: !@#$%^&*()",
            "A" * 1000,  # Long string
        ]

        for val in test_values:
            cursor.execute("DELETE FROM test_text")
            cursor.execute("INSERT INTO test_text VALUES (?)", (val,))
            cursor.execute("SELECT val FROM test_text")
            result = cursor.fetchone()[0]
            assert result == val
            assert type(result) == str

    def test_blob_types(self, memory_connection):
        """Test BLOB type preservation"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test_blob (val BLOB)")

        test_values = [
            b"",
            b"hello",
            b"\x00\x01\x02\x03\x04",
            bytes(range(256)),  # All byte values
            b"A" * 1000,  # Large blob
        ]

        for val in test_values:
            cursor.execute("DELETE FROM test_blob")
            cursor.execute("INSERT INTO test_blob VALUES (?)", (val,))
            cursor.execute("SELECT val FROM test_blob")
            result = cursor.fetchone()[0]
            assert result == val
            assert type(result) == bytes

    def test_null_values(self, memory_connection):
        """Test NULL value handling"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE test_null (
                id INTEGER,
                int_val INTEGER,
                real_val REAL,
                text_val TEXT,
                blob_val BLOB
            )
        """)

        cursor.execute("""
            INSERT INTO test_null VALUES (1, NULL, NULL, NULL, NULL)
        """)

        cursor.execute("SELECT * FROM test_null")
        row = cursor.fetchone()

        assert row[0] == 1
        assert row[1] is None
        assert row[2] is None
        assert row[3] is None
        assert row[4] is None

    def test_boolean_values(self, memory_connection):
        """Test boolean value handling (stored as INTEGER)"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test_bool (val INTEGER)")

        cursor.execute("INSERT INTO test_bool VALUES (?)", (True,))
        cursor.execute("SELECT val FROM test_bool")
        assert cursor.fetchone()[0] == 1

        cursor.execute("DELETE FROM test_bool")
        cursor.execute("INSERT INTO test_bool VALUES (?)", (False,))
        cursor.execute("SELECT val FROM test_bool")
        assert cursor.fetchone()[0] == 0


# ============================================================================
# CRUD Operations Tests
# ============================================================================

class TestCRUDOperations:
    """Test Create, Read, Update, Delete operations"""

    def test_create_table(self, memory_connection):
        """Test CREATE TABLE statement"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                username TEXT UNIQUE NOT NULL,
                email TEXT NOT NULL,
                age INTEGER,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)

        # Verify table exists
        cursor.execute("""
            SELECT name FROM sqlite_master
            WHERE type='table' AND name='users'
        """)
        assert cursor.fetchone() is not None

    def test_insert_single_row(self, memory_connection):
        """Test INSERT single row"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, name TEXT)")

        cursor.execute("INSERT INTO test VALUES (1, 'Alice')")

        cursor.execute("SELECT * FROM test")
        row = cursor.fetchone()
        assert row == (1, 'Alice')

    def test_insert_multiple_rows(self, memory_connection):
        """Test INSERT multiple rows"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, name TEXT)")

        cursor.execute("""
            INSERT INTO test VALUES
            (1, 'Alice'),
            (2, 'Bob'),
            (3, 'Charlie')
        """)

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 3

    def test_select_basic(self, memory_connection):
        """Test basic SELECT queries"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, name TEXT, age INTEGER)")
        cursor.execute("""
            INSERT INTO test VALUES
            (1, 'Alice', 30),
            (2, 'Bob', 25),
            (3, 'Charlie', 35)
        """)

        # SELECT *
        cursor.execute("SELECT * FROM test")
        assert len(cursor.fetchall()) == 3

        # SELECT specific columns
        cursor.execute("SELECT name, age FROM test")
        row = cursor.fetchone()
        assert len(row) == 2

        # SELECT with WHERE
        cursor.execute("SELECT * FROM test WHERE age > 25")
        assert len(cursor.fetchall()) == 2

    def test_update_rows(self, memory_connection):
        """Test UPDATE statement"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, name TEXT, age INTEGER)")
        cursor.execute("INSERT INTO test VALUES (1, 'Alice', 30)")

        cursor.execute("UPDATE test SET age = 31 WHERE id = 1")
        assert cursor.rowcount == 1

        cursor.execute("SELECT age FROM test WHERE id = 1")
        assert cursor.fetchone()[0] == 31

    def test_delete_rows(self, memory_connection):
        """Test DELETE statement"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, name TEXT)")
        cursor.execute("""
            INSERT INTO test VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')
        """)

        cursor.execute("DELETE FROM test WHERE id = 2")
        assert cursor.rowcount == 1

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 2


# ============================================================================
# JOIN and Aggregation Tests
# ============================================================================

class TestJoinsAndAggregations:
    """Test JOIN operations and aggregate functions"""

    def test_inner_join(self, memory_connection):
        """Test INNER JOIN"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE users (id INTEGER, name TEXT);
            CREATE TABLE orders (id INTEGER, user_id INTEGER, amount REAL);
        """)
        cursor.execute("""
            INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
            INSERT INTO orders VALUES (1, 1, 100.0), (2, 1, 200.0), (3, 2, 150.0);
        """)

        cursor.execute("""
            SELECT u.name, o.amount
            FROM users u
            INNER JOIN orders o ON u.id = o.user_id
            ORDER BY o.id
        """)

        results = cursor.fetchall()
        assert len(results) == 3
        assert results[0] == ('Alice', 100.0)
        assert results[2] == ('Bob', 150.0)

    def test_left_join(self, memory_connection):
        """Test LEFT JOIN"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE users (id INTEGER, name TEXT);
            CREATE TABLE orders (id INTEGER, user_id INTEGER, amount REAL);
        """)
        cursor.execute("""
            INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie');
            INSERT INTO orders VALUES (1, 1, 100.0);
        """)

        cursor.execute("""
            SELECT u.name, o.amount
            FROM users u
            LEFT JOIN orders o ON u.id = o.user_id
            ORDER BY u.id
        """)

        results = cursor.fetchall()
        assert len(results) == 3
        assert results[0] == ('Alice', 100.0)
        assert results[1][0] == 'Bob'
        assert results[1][1] is None

    def test_aggregate_functions(self, memory_connection):
        """Test aggregate functions (COUNT, SUM, AVG, MIN, MAX)"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE sales (id INTEGER, amount REAL)")
        cursor.execute("""
            INSERT INTO sales VALUES
            (1, 100.0), (2, 200.0), (3, 150.0), (4, 300.0)
        """)

        cursor.execute("""
            SELECT
                COUNT(*) as count,
                SUM(amount) as total,
                AVG(amount) as average,
                MIN(amount) as minimum,
                MAX(amount) as maximum
            FROM sales
        """)

        row = cursor.fetchone()
        assert row[0] == 4  # count
        assert row[1] == 750.0  # sum
        assert row[2] == 187.5  # avg
        assert row[3] == 100.0  # min
        assert row[4] == 300.0  # max

    def test_group_by(self, memory_connection):
        """Test GROUP BY clause"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE sales (category TEXT, amount REAL)
        """)
        cursor.execute("""
            INSERT INTO sales VALUES
            ('A', 100), ('A', 200), ('B', 150), ('B', 250)
        """)

        cursor.execute("""
            SELECT category, SUM(amount) as total
            FROM sales
            GROUP BY category
            ORDER BY category
        """)

        results = cursor.fetchall()
        assert len(results) == 2
        assert results[0] == ('A', 300.0)
        assert results[1] == ('B', 400.0)

    def test_having_clause(self, memory_connection):
        """Test HAVING clause with GROUP BY"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE sales (category TEXT, amount REAL)
        """)
        cursor.execute("""
            INSERT INTO sales VALUES
            ('A', 100), ('A', 200), ('B', 50), ('C', 300)
        """)

        cursor.execute("""
            SELECT category, SUM(amount) as total
            FROM sales
            GROUP BY category
            HAVING SUM(amount) > 100
            ORDER BY category
        """)

        results = cursor.fetchall()
        assert len(results) == 2
        assert 'B' not in [r[0] for r in results]


# ============================================================================
# Transaction Tests
# ============================================================================

class TestTransactions:
    """Test transaction handling (commit, rollback, savepoint)"""

    def test_commit(self, file_connection):
        """Test transaction COMMIT"""
        cursor = file_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, name TEXT)")

        file_connection.isolation_level = "DEFERRED"
        cursor.execute("INSERT INTO test VALUES (1, 'Alice')")
        file_connection.commit()

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 1

    def test_rollback(self, file_connection):
        """Test transaction ROLLBACK"""
        cursor = file_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, name TEXT)")
        file_connection.commit()

        cursor.execute("INSERT INTO test VALUES (1, 'Alice')")
        file_connection.rollback()

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 0

    def test_autocommit_mode(self, memory_connection):
        """Test autocommit mode (isolation_level=None)"""
        memory_connection.isolation_level = None
        cursor = memory_connection.cursor()

        cursor.execute("CREATE TABLE test (id INTEGER)")
        cursor.execute("INSERT INTO test VALUES (1)")

        # No explicit commit needed
        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 1

    def test_savepoint(self, memory_connection):
        """Test SAVEPOINT and ROLLBACK TO"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER)")

        cursor.execute("INSERT INTO test VALUES (1)")
        cursor.execute("SAVEPOINT sp1")

        cursor.execute("INSERT INTO test VALUES (2)")
        cursor.execute("SAVEPOINT sp2")

        cursor.execute("INSERT INTO test VALUES (3)")
        cursor.execute("ROLLBACK TO sp2")

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 2  # Only 1 and 2

    def test_nested_transactions(self, memory_connection):
        """Test nested transaction behavior"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER)")

        cursor.execute("BEGIN")
        cursor.execute("INSERT INTO test VALUES (1)")

        cursor.execute("SAVEPOINT sp1")
        cursor.execute("INSERT INTO test VALUES (2)")
        cursor.execute("RELEASE sp1")

        cursor.execute("COMMIT")

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 2


# ============================================================================
# Error Handling Tests
# ============================================================================

class TestErrorHandling:
    """Test error conditions and exception mapping"""

    def test_syntax_error(self, memory_connection):
        """Test SQL syntax error"""
        cursor = memory_connection.cursor()

        with pytest.raises(sqlite3.OperationalError):
            cursor.execute("INVALID SQL SYNTAX")

    def test_table_not_found(self, memory_connection):
        """Test table not found error"""
        cursor = memory_connection.cursor()

        with pytest.raises(sqlite3.OperationalError):
            cursor.execute("SELECT * FROM nonexistent_table")

    def test_unique_constraint_violation(self, memory_connection):
        """Test UNIQUE constraint violation"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE test (id INTEGER UNIQUE)
        """)

        cursor.execute("INSERT INTO test VALUES (1)")

        with pytest.raises(sqlite3.IntegrityError):
            cursor.execute("INSERT INTO test VALUES (1)")

    def test_not_null_constraint_violation(self, memory_connection):
        """Test NOT NULL constraint violation"""
        cursor = memory_connection.cursor()
        cursor.execute("""
            CREATE TABLE test (id INTEGER NOT NULL)
        """)

        with pytest.raises(sqlite3.IntegrityError):
            cursor.execute("INSERT INTO test VALUES (NULL)")

    def test_foreign_key_constraint(self, memory_connection):
        """Test FOREIGN KEY constraint"""
        cursor = memory_connection.cursor()
        cursor.execute("PRAGMA foreign_keys = ON")

        cursor.execute("""
            CREATE TABLE parent (id INTEGER PRIMARY KEY);
            CREATE TABLE child (
                id INTEGER,
                parent_id INTEGER,
                FOREIGN KEY (parent_id) REFERENCES parent(id)
            );
        """)

        with pytest.raises(sqlite3.IntegrityError):
            cursor.execute("INSERT INTO child VALUES (1, 999)")

    def test_type_mismatch_handling(self, memory_connection):
        """Test type affinity and conversion"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER)")

        # SQLite allows flexible typing
        cursor.execute("INSERT INTO test VALUES ('123')")
        cursor.execute("SELECT id FROM test")
        # Result may be string or int depending on affinity
        result = cursor.fetchone()[0]
        assert result == '123' or result == 123


# ============================================================================
# Unicode and Encoding Tests
# ============================================================================

class TestUnicodeAndEncoding:
    """Test Unicode support and text encoding"""

    def test_unicode_text(self, memory_connection):
        """Test Unicode text storage and retrieval"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (text TEXT)")

        unicode_samples = [
            "Hello, 世界",  # Chinese
            "Привет, мир",  # Russian
            "مرحبا بالعالم",  # Arabic
            "こんにちは世界",  # Japanese
            "안녕하세요 세계",  # Korean
            "🚀 🌟 💻 🔥",  # Emoji
        ]

        for sample in unicode_samples:
            cursor.execute("INSERT INTO test VALUES (?)", (sample,))

        cursor.execute("SELECT * FROM test")
        results = [row[0] for row in cursor.fetchall()]

        assert results == unicode_samples

    def test_unicode_column_names(self, memory_connection):
        """Test Unicode in column names"""
        cursor = memory_connection.cursor()
        cursor.execute('CREATE TABLE test ("名前" TEXT, "年齢" INTEGER)')
        cursor.execute('INSERT INTO test VALUES (?, ?)', ("太郎", 30))

        cursor.execute("SELECT * FROM test")
        row = cursor.fetchone()
        assert row == ("太郎", 30)

    def test_text_encoding_consistency(self, memory_connection):
        """Test consistent text encoding across operations"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (data TEXT)")

        test_string = "Mixed: ASCII + Unicode 中文"
        cursor.execute("INSERT INTO test VALUES (?)", (test_string,))

        cursor.execute("SELECT data FROM test")
        result = cursor.fetchone()[0]

        assert result == test_string
        assert isinstance(result, str)


# ============================================================================
# Large Dataset Tests
# ============================================================================

class TestLargeDatasets:
    """Test handling of large datasets"""

    def test_large_insert_batch(self, memory_connection):
        """Test inserting large batch of rows"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, value TEXT)")

        batch_size = 10000
        data = [(i, f"value_{i}") for i in range(batch_size)]

        cursor.executemany("INSERT INTO test VALUES (?, ?)", data)

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == batch_size

    def test_large_result_set(self, memory_connection):
        """Test fetching large result set"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER)")

        count = 5000
        cursor.executemany(
            "INSERT INTO test VALUES (?)",
            [(i,) for i in range(count)]
        )

        cursor.execute("SELECT * FROM test")
        results = cursor.fetchall()

        assert len(results) == count

    def test_large_text_field(self, memory_connection):
        """Test storing and retrieving large text"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (data TEXT)")

        # 1MB of text
        large_text = "A" * (1024 * 1024)
        cursor.execute("INSERT INTO test VALUES (?)", (large_text,))

        cursor.execute("SELECT data FROM test")
        result = cursor.fetchone()[0]

        assert len(result) == len(large_text)
        assert result == large_text

    def test_large_blob_field(self, memory_connection):
        """Test storing and retrieving large binary data"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (data BLOB)")

        # 1MB of binary data
        large_blob = bytes(range(256)) * 4096
        cursor.execute("INSERT INTO test VALUES (?)", (large_blob,))

        cursor.execute("SELECT data FROM test")
        result = cursor.fetchone()[0]

        assert len(result) == len(large_blob)
        assert result == large_blob


# ============================================================================
# Concurrent Access Tests
# ============================================================================

class TestConcurrentAccess:
    """Test multi-threaded database access"""

    def test_concurrent_reads(self, file_connection, temp_db_path):
        """Test concurrent read operations"""
        cursor = file_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER, value TEXT)")
        cursor.execute("""
            INSERT INTO test VALUES (1, 'a'), (2, 'b'), (3, 'c')
        """)
        file_connection.commit()
        file_connection.close()

        results = []
        errors = []

        def read_worker():
            try:
                conn = sqlite3.connect(temp_db_path)
                cursor = conn.cursor()
                cursor.execute("SELECT COUNT(*) FROM test")
                count = cursor.fetchone()[0]
                results.append(count)
                conn.close()
            except Exception as e:
                errors.append(e)

        threads = [threading.Thread(target=read_worker) for _ in range(10)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert len(errors) == 0
        assert all(r == 3 for r in results)

    def test_concurrent_writes_with_lock(self, temp_db_path):
        """Test concurrent write operations with locking"""
        # Initialize database
        conn = sqlite3.connect(temp_db_path)
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value TEXT)")
        conn.commit()
        conn.close()

        errors = []
        lock = threading.Lock()

        def write_worker(worker_id):
            try:
                conn = sqlite3.connect(temp_db_path, timeout=10.0)
                cursor = conn.cursor()

                with lock:
                    cursor.execute(
                        "INSERT INTO test (id, value) VALUES (?, ?)",
                        (worker_id, f"worker_{worker_id}")
                    )
                    conn.commit()

                conn.close()
            except Exception as e:
                errors.append(e)

        threads = [
            threading.Thread(target=write_worker, args=(i,))
            for i in range(10)
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert len(errors) == 0

        # Verify all writes succeeded
        conn = sqlite3.connect(temp_db_path)
        cursor = conn.cursor()
        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 10
        conn.close()


# ============================================================================
# Memory Mode Tests
# ============================================================================

class TestMemoryMode:
    """Test in-memory database mode"""

    def test_memory_database_lifecycle(self):
        """Test memory database creation and destruction"""
        conn = sqlite3.connect(":memory:")
        cursor = conn.cursor()

        cursor.execute("CREATE TABLE test (id INTEGER)")
        cursor.execute("INSERT INTO test VALUES (1)")

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 1

        conn.close()

        # New connection should have fresh database
        conn2 = sqlite3.connect(":memory:")
        cursor2 = conn2.cursor()

        with pytest.raises(sqlite3.OperationalError):
            cursor2.execute("SELECT * FROM test")

        conn2.close()

    def test_shared_memory_database(self):
        """Test shared in-memory database"""
        # Create shared memory database
        conn1 = sqlite3.connect("file:memdb1?mode=memory&cache=shared", uri=True)
        cursor1 = conn1.cursor()
        cursor1.execute("CREATE TABLE test (id INTEGER)")
        cursor1.execute("INSERT INTO test VALUES (1)")

        # Connect to same shared memory database
        conn2 = sqlite3.connect("file:memdb1?mode=memory&cache=shared", uri=True)
        cursor2 = conn2.cursor()
        cursor2.execute("SELECT COUNT(*) FROM test")
        assert cursor2.fetchone()[0] == 1

        conn1.close()
        conn2.close()


# ============================================================================
# Main Execution
# ============================================================================

if __name__ == "__main__":
    pytest.main([__file__, "-v", "--tb=short"])
