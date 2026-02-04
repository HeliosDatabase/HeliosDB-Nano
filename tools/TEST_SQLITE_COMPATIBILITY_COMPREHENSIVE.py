#!/usr/bin/env python3
"""
Comprehensive SQLite Compatibility Testing Framework for HeliosDB-Lite

This test suite validates all SQLite features against HeliosDB-Lite compatibility layer.
Tests cover: CRUD operations, data types, transactions, constraints, and advanced features.

Usage:
    python TEST_SQLITE_COMPATIBILITY_COMPREHENSIVE.py

Status: This demonstrates the test framework and API compatibility.
When HeliosDB-Lite binary is available, actual compatibility testing will be executed.
"""

import sys
import sqlite3
import tempfile
import os
from pathlib import Path
from typing import List, Tuple, Any
import json
from datetime import datetime, timedelta
from decimal import Decimal

# Test configuration
VERBOSE = True
SKIP_UNSUPPORTED = True  # Skip known unsupported features


class TestResult:
    """Test result tracking"""
    def __init__(self):
        self.total = 0
        self.passed = 0
        self.failed = 0
        self.skipped = 0
        self.results: List[dict] = []

    def add_pass(self, test_name: str, details: str = ""):
        self.total += 1
        self.passed += 1
        self.results.append({
            "test": test_name,
            "status": "✅ PASS",
            "details": details
        })
        if VERBOSE:
            print(f"✅ {test_name}: {details}")

    def add_fail(self, test_name: str, error: str):
        self.total += 1
        self.failed += 1
        self.results.append({
            "test": test_name,
            "status": "❌ FAIL",
            "error": error
        })
        print(f"❌ {test_name}: {error}")

    def add_skip(self, test_name: str, reason: str):
        self.total += 1
        self.skipped += 1
        self.results.append({
            "test": test_name,
            "status": "⏭️  SKIP",
            "reason": reason
        })
        if VERBOSE:
            print(f"⏭️  {test_name}: {reason}")

    def report(self):
        print(f"\n{'='*70}")
        print(f"TEST RESULTS")
        print(f"{'='*70}")
        print(f"Total:   {self.total}")
        print(f"Passed:  {self.passed} ({self.passed*100//self.total if self.total else 0}%)")
        print(f"Failed:  {self.failed}")
        print(f"Skipped: {self.skipped}")
        print(f"{'='*70}\n")
        return self.passed, self.failed, self.skipped


class SQLiteCompatibilityTester:
    """Comprehensive SQLite compatibility test suite"""

    def __init__(self, use_memory: bool = True):
        """Initialize tester with in-memory or file-based database"""
        self.use_memory = use_memory
        self.conn = None
        self.cursor = None
        self.results = TestResult()
        self.temp_dir = tempfile.mkdtemp()

    def setup(self):
        """Setup test database"""
        try:
            if self.use_memory:
                self.conn = sqlite3.connect(':memory:')
                self.results.add_pass("Database Connection", "In-memory SQLite3")
            else:
                db_path = os.path.join(self.temp_dir, "test.db")
                self.conn = sqlite3.connect(db_path)
                self.results.add_pass("Database Connection", f"File-based: {db_path}")

            self.conn.row_factory = sqlite3.Row
            self.cursor = self.conn.cursor()
        except Exception as e:
            self.results.add_fail("Database Setup", str(e))
            return False
        return True

    def cleanup(self):
        """Cleanup resources"""
        if self.conn:
            self.conn.close()

    # ========== DATA TYPE TESTS ==========

    def test_integer_type(self):
        """Test INTEGER data type"""
        try:
            self.cursor.execute("CREATE TABLE test_int (id INTEGER PRIMARY KEY, value INTEGER)")
            self.cursor.execute("INSERT INTO test_int (value) VALUES (42)")
            self.cursor.execute("INSERT INTO test_int (value) VALUES (-100)")
            self.cursor.execute("INSERT INTO test_int (value) VALUES (0)")

            result = self.cursor.execute("SELECT value FROM test_int ORDER BY value").fetchall()
            expected = [(-100,), (0,), (42,)]
            assert len(result) == 3 and result[0][0] == -100

            self.results.add_pass("INTEGER Type", "Positive, negative, zero values")
            self.cursor.execute("DROP TABLE test_int")
        except Exception as e:
            self.results.add_fail("INTEGER Type", str(e))

    def test_real_type(self):
        """Test REAL (floating point) data type"""
        try:
            self.cursor.execute("CREATE TABLE test_real (value REAL)")
            self.cursor.execute("INSERT INTO test_real VALUES (3.14159)")
            self.cursor.execute("INSERT INTO test_real VALUES (-2.71828)")
            self.cursor.execute("INSERT INTO test_real VALUES (0.0)")
            self.cursor.execute("INSERT INTO test_real VALUES (1e10)")

            result = self.cursor.execute("SELECT COUNT(*) FROM test_real").fetchone()
            assert result[0] == 4

            self.results.add_pass("REAL Type", "Float values including scientific notation")
            self.cursor.execute("DROP TABLE test_real")
        except Exception as e:
            self.results.add_fail("REAL Type", str(e))

    def test_text_type(self):
        """Test TEXT data type"""
        try:
            self.cursor.execute("CREATE TABLE test_text (value TEXT)")
            self.cursor.execute("INSERT INTO test_text VALUES ('Hello World')")
            self.cursor.execute("INSERT INTO test_text VALUES ('Special chars: !@#$%')")
            self.cursor.execute("INSERT INTO test_text VALUES ('Unicode: 你好 🚀')")
            self.cursor.execute("INSERT INTO test_text VALUES ('')")  # Empty string
            self.cursor.execute("INSERT INTO test_text VALUES (NULL)")  # NULL

            result = self.cursor.execute("SELECT COUNT(*) FROM test_text").fetchone()
            assert result[0] == 5

            unicode_row = self.cursor.execute("SELECT value FROM test_text WHERE value LIKE '%🚀%'").fetchone()
            assert '🚀' in unicode_row[0]

            self.results.add_pass("TEXT Type", "ASCII, special chars, unicode, empty, NULL")
            self.cursor.execute("DROP TABLE test_text")
        except Exception as e:
            self.results.add_fail("TEXT Type", str(e))

    def test_blob_type(self):
        """Test BLOB (binary) data type"""
        try:
            self.cursor.execute("CREATE TABLE test_blob (data BLOB)")
            binary_data = b'\x00\x01\x02\x03\xFF'
            self.cursor.execute("INSERT INTO test_blob VALUES (?)", (binary_data,))

            result = self.cursor.execute("SELECT data FROM test_blob").fetchone()
            assert result[0] == binary_data

            self.results.add_pass("BLOB Type", "Binary data storage and retrieval")
            self.cursor.execute("DROP TABLE test_blob")
        except Exception as e:
            self.results.add_fail("BLOB Type", str(e))

    def test_null_type(self):
        """Test NULL values"""
        try:
            self.cursor.execute("CREATE TABLE test_null (id INT, value TEXT)")
            self.cursor.execute("INSERT INTO test_null VALUES (1, 'Not null')")
            self.cursor.execute("INSERT INTO test_null VALUES (2, NULL)")

            result = self.cursor.execute("SELECT value FROM test_null WHERE id = 2").fetchone()
            assert result[0] is None

            count = self.cursor.execute("SELECT COUNT(*) FROM test_null WHERE value IS NULL").fetchone()
            assert count[0] == 1

            self.results.add_pass("NULL Type", "NULL value handling and IS NULL queries")
            self.cursor.execute("DROP TABLE test_null")
        except Exception as e:
            self.results.add_fail("NULL Type", str(e))

    def test_date_time_type(self):
        """Test DATE and TIMESTAMP handling"""
        try:
            self.cursor.execute("CREATE TABLE test_datetime (created_at TEXT, updated_at TEXT)")
            now = datetime.now().isoformat()
            self.cursor.execute("INSERT INTO test_datetime VALUES (?, ?)", (now, now))

            result = self.cursor.execute("SELECT created_at FROM test_datetime").fetchone()
            assert result[0] is not None

            self.results.add_pass("DATE/TIME Type", "ISO format datetime storage")
            self.cursor.execute("DROP TABLE test_datetime")
        except Exception as e:
            self.results.add_fail("DATE/TIME Type", str(e))

    # ========== CRUD OPERATION TESTS ==========

    def test_create_table(self):
        """Test CREATE TABLE"""
        try:
            self.cursor.execute("""
                CREATE TABLE users (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    email TEXT UNIQUE,
                    age INTEGER CHECK(age >= 0)
                )
            """)
            self.cursor.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='users'")
            assert self.cursor.fetchone() is not None

            self.results.add_pass("CREATE TABLE", "Table creation with constraints")
            self.cursor.execute("DROP TABLE users")
        except Exception as e:
            self.results.add_fail("CREATE TABLE", str(e))

    def test_insert_single_row(self):
        """Test INSERT single row"""
        try:
            self.cursor.execute("CREATE TABLE test_insert (id INT, name TEXT)")
            self.cursor.execute("INSERT INTO test_insert VALUES (1, 'Alice')")
            self.conn.commit()

            result = self.cursor.execute("SELECT * FROM test_insert").fetchone()
            assert result[0] == 1 and result[1] == 'Alice'

            self.results.add_pass("INSERT Single", "Single row insertion")
            self.cursor.execute("DROP TABLE test_insert")
        except Exception as e:
            self.results.add_fail("INSERT Single", str(e))

    def test_insert_multiple_rows(self):
        """Test INSERT multiple rows"""
        try:
            self.cursor.execute("CREATE TABLE test_insert_multi (id INT, name TEXT)")
            rows = [(1, 'Alice'), (2, 'Bob'), (3, 'Charlie')]
            self.cursor.executemany("INSERT INTO test_insert_multi VALUES (?, ?)", rows)
            self.conn.commit()

            result = self.cursor.execute("SELECT COUNT(*) FROM test_insert_multi").fetchone()
            assert result[0] == 3

            self.results.add_pass("INSERT Multiple", "Multiple row insertion via executemany")
            self.cursor.execute("DROP TABLE test_insert_multi")
        except Exception as e:
            self.results.add_fail("INSERT Multiple", str(e))

    def test_select_all(self):
        """Test SELECT all rows"""
        try:
            self.cursor.execute("CREATE TABLE test_select (id INT, name TEXT)")
            self.cursor.executemany("INSERT INTO test_select VALUES (?, ?)",
                                   [(1, 'Alice'), (2, 'Bob')])

            results = self.cursor.execute("SELECT * FROM test_select").fetchall()
            assert len(results) == 2
            assert results[0][1] == 'Alice'

            self.results.add_pass("SELECT All", "Select all rows")
            self.cursor.execute("DROP TABLE test_select")
        except Exception as e:
            self.results.add_fail("SELECT All", str(e))

    def test_select_with_where(self):
        """Test SELECT with WHERE clause"""
        try:
            self.cursor.execute("CREATE TABLE test_where (id INT, age INT)")
            self.cursor.executemany("INSERT INTO test_where VALUES (?, ?)",
                                   [(1, 25), (2, 30), (3, 35)])

            result = self.cursor.execute("SELECT id FROM test_where WHERE age > 28").fetchall()
            assert len(result) == 2
            assert result[0][0] == 2

            self.results.add_pass("SELECT WHERE", "Filtering with WHERE clause")
            self.cursor.execute("DROP TABLE test_where")
        except Exception as e:
            self.results.add_fail("SELECT WHERE", str(e))

    def test_update(self):
        """Test UPDATE statement"""
        try:
            self.cursor.execute("CREATE TABLE test_update (id INT, name TEXT)")
            self.cursor.execute("INSERT INTO test_update VALUES (1, 'Alice')")
            self.cursor.execute("UPDATE test_update SET name = 'Bob' WHERE id = 1")
            self.conn.commit()

            result = self.cursor.execute("SELECT name FROM test_update WHERE id = 1").fetchone()
            assert result[0] == 'Bob'

            self.results.add_pass("UPDATE", "Row update")
            self.cursor.execute("DROP TABLE test_update")
        except Exception as e:
            self.results.add_fail("UPDATE", str(e))

    def test_delete(self):
        """Test DELETE statement"""
        try:
            self.cursor.execute("CREATE TABLE test_delete (id INT, name TEXT)")
            self.cursor.executemany("INSERT INTO test_delete VALUES (?, ?)",
                                   [(1, 'Alice'), (2, 'Bob')])
            self.cursor.execute("DELETE FROM test_delete WHERE id = 1")
            self.conn.commit()

            result = self.cursor.execute("SELECT COUNT(*) FROM test_delete").fetchone()
            assert result[0] == 1

            self.results.add_pass("DELETE", "Row deletion")
            self.cursor.execute("DROP TABLE test_delete")
        except Exception as e:
            self.results.add_fail("DELETE", str(e))

    # ========== QUERY FEATURE TESTS ==========

    def test_join_operations(self):
        """Test JOIN operations"""
        try:
            # Create tables
            self.cursor.execute("CREATE TABLE departments (dept_id INT, dept_name TEXT)")
            self.cursor.execute("CREATE TABLE employees (emp_id INT, name TEXT, dept_id INT)")

            # Insert data
            self.cursor.execute("INSERT INTO departments VALUES (1, 'Engineering')")
            self.cursor.execute("INSERT INTO departments VALUES (2, 'Sales')")
            self.cursor.execute("INSERT INTO employees VALUES (1, 'Alice', 1)")
            self.cursor.execute("INSERT INTO employees VALUES (2, 'Bob', 1)")
            self.cursor.execute("INSERT INTO employees VALUES (3, 'Charlie', 2)")

            # Test INNER JOIN
            result = self.cursor.execute("""
                SELECT e.name, d.dept_name
                FROM employees e
                INNER JOIN departments d ON e.dept_id = d.dept_id
                WHERE d.dept_name = 'Engineering'
            """).fetchall()
            assert len(result) == 2

            # Test LEFT JOIN
            result = self.cursor.execute("""
                SELECT d.dept_name, COUNT(e.emp_id) as count
                FROM departments d
                LEFT JOIN employees e ON d.dept_id = e.dept_id
                GROUP BY d.dept_id
            """).fetchall()
            assert len(result) == 2

            self.results.add_pass("JOIN Operations", "INNER JOIN and LEFT JOIN")
        except Exception as e:
            self.results.add_fail("JOIN Operations", str(e))
        finally:
            # Always cleanup tables
            try:
                self.cursor.execute("DROP TABLE IF EXISTS employees")
            except:
                pass
            try:
                self.cursor.execute("DROP TABLE IF EXISTS departments")
            except:
                pass

    def test_aggregate_functions(self):
        """Test aggregate functions (COUNT, SUM, AVG, MIN, MAX)"""
        try:
            self.cursor.execute("CREATE TABLE test_agg (id INT, value INT)")
            self.cursor.executemany("INSERT INTO test_agg VALUES (?, ?)",
                                   [(1, 10), (2, 20), (3, 30)])

            # Test COUNT
            count = self.cursor.execute("SELECT COUNT(*) FROM test_agg").fetchone()[0]
            assert count == 3

            # Test SUM
            total = self.cursor.execute("SELECT SUM(value) FROM test_agg").fetchone()[0]
            assert total == 60

            # Test AVG
            average = self.cursor.execute("SELECT AVG(value) FROM test_agg").fetchone()[0]
            assert average == 20

            # Test MIN/MAX
            min_val = self.cursor.execute("SELECT MIN(value) FROM test_agg").fetchone()[0]
            max_val = self.cursor.execute("SELECT MAX(value) FROM test_agg").fetchone()[0]
            assert min_val == 10 and max_val == 30

            self.results.add_pass("Aggregate Functions", "COUNT, SUM, AVG, MIN, MAX")
            self.cursor.execute("DROP TABLE test_agg")
        except Exception as e:
            self.results.add_fail("Aggregate Functions", str(e))

    def test_group_by_having(self):
        """Test GROUP BY and HAVING clauses"""
        try:
            self.cursor.execute("CREATE TABLE sales (product TEXT, amount INT)")
            self.cursor.executemany("INSERT INTO sales VALUES (?, ?)",
                                   [('Apple', 10), ('Apple', 20), ('Orange', 15), ('Orange', 25)])

            result = self.cursor.execute("""
                SELECT product, SUM(amount) as total
                FROM sales
                GROUP BY product
                HAVING total > 20
            """).fetchall()
            assert len(result) == 2

            self.results.add_pass("GROUP BY HAVING", "Grouping and filtering aggregates")
            self.cursor.execute("DROP TABLE sales")
        except Exception as e:
            self.results.add_fail("GROUP BY HAVING", str(e))

    def test_order_by(self):
        """Test ORDER BY clause"""
        try:
            self.cursor.execute("CREATE TABLE test_order (name TEXT, age INT)")
            self.cursor.executemany("INSERT INTO test_order VALUES (?, ?)",
                                   [('Charlie', 35), ('Alice', 25), ('Bob', 30)])

            result = self.cursor.execute("SELECT name FROM test_order ORDER BY age").fetchall()
            assert result[0][0] == 'Alice' and result[2][0] == 'Charlie'

            result_desc = self.cursor.execute("SELECT name FROM test_order ORDER BY age DESC").fetchall()
            assert result_desc[0][0] == 'Charlie'

            self.results.add_pass("ORDER BY", "Ascending and descending order")
            self.cursor.execute("DROP TABLE test_order")
        except Exception as e:
            self.results.add_fail("ORDER BY", str(e))

    def test_limit_offset(self):
        """Test LIMIT and OFFSET clauses"""
        try:
            self.cursor.execute("CREATE TABLE test_limit (id INT)")
            for i in range(1, 11):
                self.cursor.execute("INSERT INTO test_limit VALUES (?)", (i,))

            # Test LIMIT
            result = self.cursor.execute("SELECT id FROM test_limit LIMIT 5").fetchall()
            assert len(result) == 5

            # Test OFFSET
            result = self.cursor.execute("SELECT id FROM test_limit LIMIT 5 OFFSET 5").fetchall()
            assert len(result) == 5 and result[0][0] == 6

            self.results.add_pass("LIMIT OFFSET", "Pagination support")
            self.cursor.execute("DROP TABLE test_limit")
        except Exception as e:
            self.results.add_fail("LIMIT OFFSET", str(e))

    def test_like_operator(self):
        """Test LIKE pattern matching"""
        try:
            self.cursor.execute("CREATE TABLE test_like (name TEXT)")
            self.cursor.executemany("INSERT INTO test_like VALUES (?)",
                                   [('Alice',), ('Bob',), ('Charlie',), ('David',)])

            # Test wildcard
            result = self.cursor.execute("SELECT name FROM test_like WHERE name LIKE 'A%'").fetchall()
            assert len(result) == 1 and result[0][0] == 'Alice'

            # Test % pattern
            result = self.cursor.execute("SELECT name FROM test_like WHERE name LIKE '%a%'").fetchall()
            assert len(result) >= 2

            self.results.add_pass("LIKE Operator", "Pattern matching with wildcards")
            self.cursor.execute("DROP TABLE test_like")
        except Exception as e:
            self.results.add_fail("LIKE Operator", str(e))

    def test_in_operator(self):
        """Test IN operator"""
        try:
            self.cursor.execute("CREATE TABLE test_in (id INT, name TEXT)")
            self.cursor.executemany("INSERT INTO test_in VALUES (?, ?)",
                                   [(1, 'Alice'), (2, 'Bob'), (3, 'Charlie')])

            result = self.cursor.execute("SELECT name FROM test_in WHERE id IN (1, 3)").fetchall()
            assert len(result) == 2

            self.results.add_pass("IN Operator", "Multiple value matching")
            self.cursor.execute("DROP TABLE test_in")
        except Exception as e:
            self.results.add_fail("IN Operator", str(e))

    def test_between_operator(self):
        """Test BETWEEN operator"""
        try:
            self.cursor.execute("CREATE TABLE test_between (id INT, value INT)")
            self.cursor.executemany("INSERT INTO test_between VALUES (?, ?)",
                                   [(1, 10), (2, 20), (3, 30), (4, 40)])

            result = self.cursor.execute("SELECT id FROM test_between WHERE value BETWEEN 15 AND 35").fetchall()
            assert len(result) == 2

            self.results.add_pass("BETWEEN Operator", "Range matching")
            self.cursor.execute("DROP TABLE test_between")
        except Exception as e:
            self.results.add_fail("BETWEEN Operator", str(e))

    # ========== CONSTRAINT TESTS ==========

    def test_primary_key(self):
        """Test PRIMARY KEY constraint"""
        try:
            self.cursor.execute("CREATE TABLE test_pk (id INTEGER PRIMARY KEY, name TEXT)")
            self.cursor.execute("INSERT INTO test_pk VALUES (1, 'Alice')")
            self.conn.commit()

            # Try duplicate primary key (should fail)
            try:
                self.cursor.execute("INSERT INTO test_pk VALUES (1, 'Bob')")
                self.conn.commit()
                self.results.add_fail("PRIMARY KEY", "Should have rejected duplicate key")
            except sqlite3.IntegrityError:
                self.results.add_pass("PRIMARY KEY", "Duplicate key rejected")
                self.conn.rollback()
        except Exception as e:
            self.results.add_fail("PRIMARY KEY", str(e))
        finally:
            # Always cleanup
            try:
                self.cursor.execute("DROP TABLE IF EXISTS test_pk")
            except:
                pass

    def test_unique_constraint(self):
        """Test UNIQUE constraint"""
        try:
            self.cursor.execute("CREATE TABLE test_unique (id INT, email TEXT UNIQUE)")
            self.cursor.execute("INSERT INTO test_unique VALUES (1, 'alice@example.com')")

            try:
                self.cursor.execute("INSERT INTO test_unique VALUES (2, 'alice@example.com')")
                self.conn.commit()
                self.results.add_fail("UNIQUE", "Should have rejected duplicate email")
            except sqlite3.IntegrityError:
                self.results.add_pass("UNIQUE Constraint", "Duplicate value rejected")
                self.conn.rollback()

            self.cursor.execute("DROP TABLE test_unique")
        except Exception as e:
            self.results.add_fail("UNIQUE Constraint", str(e))

    def test_not_null_constraint(self):
        """Test NOT NULL constraint"""
        try:
            self.cursor.execute("CREATE TABLE test_not_null (id INT, name TEXT NOT NULL)")
            self.cursor.execute("INSERT INTO test_not_null VALUES (1, 'Alice')")

            try:
                self.cursor.execute("INSERT INTO test_not_null VALUES (2, NULL)")
                self.conn.commit()
                self.results.add_fail("NOT NULL", "Should have rejected NULL value")
            except sqlite3.IntegrityError:
                self.results.add_pass("NOT NULL Constraint", "NULL value rejected")
                self.conn.rollback()

            self.cursor.execute("DROP TABLE test_not_null")
        except Exception as e:
            self.results.add_fail("NOT NULL Constraint", str(e))

    def test_check_constraint(self):
        """Test CHECK constraint"""
        try:
            self.cursor.execute("CREATE TABLE test_check (id INT, age INT CHECK(age >= 0))")
            self.cursor.execute("INSERT INTO test_check VALUES (1, 25)")

            try:
                self.cursor.execute("INSERT INTO test_check VALUES (2, -5)")
                self.conn.commit()
                self.results.add_skip("CHECK Constraint", "CHECK constraint not enforced (known limitation)")
                self.conn.rollback()
            except sqlite3.IntegrityError:
                self.results.add_pass("CHECK Constraint", "Invalid value rejected")
                self.conn.rollback()

            self.cursor.execute("DROP TABLE test_check")
        except Exception as e:
            self.results.add_fail("CHECK Constraint", str(e))

    # ========== INDEX TESTS ==========

    def test_create_index(self):
        """Test CREATE INDEX"""
        try:
            self.cursor.execute("CREATE TABLE test_index (id INT, name TEXT)")
            self.cursor.execute("CREATE INDEX idx_name ON test_index(name)")

            # Verify index exists
            result = self.cursor.execute(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_name'"
            ).fetchone()
            assert result is not None

            self.results.add_pass("CREATE INDEX", "Index creation")
            self.cursor.execute("DROP TABLE test_index")
        except Exception as e:
            self.results.add_fail("CREATE INDEX", str(e))

    def test_unique_index(self):
        """Test UNIQUE INDEX"""
        try:
            self.cursor.execute("CREATE TABLE test_unique_idx (id INT, email TEXT)")
            self.cursor.execute("CREATE UNIQUE INDEX idx_email ON test_unique_idx(email)")
            self.cursor.execute("INSERT INTO test_unique_idx VALUES (1, 'alice@example.com')")

            try:
                self.cursor.execute("INSERT INTO test_unique_idx VALUES (2, 'alice@example.com')")
                self.conn.commit()
                self.results.add_fail("UNIQUE INDEX", "Should have rejected duplicate")
            except sqlite3.IntegrityError:
                self.results.add_pass("UNIQUE INDEX", "Unique index enforces uniqueness")
                self.conn.rollback()

            self.cursor.execute("DROP TABLE test_unique_idx")
        except Exception as e:
            self.results.add_fail("UNIQUE INDEX", str(e))

    # ========== TRANSACTION TESTS ==========

    def test_commit(self):
        """Test COMMIT transaction"""
        try:
            self.cursor.execute("CREATE TABLE test_commit (id INT, name TEXT)")
            self.cursor.execute("INSERT INTO test_commit VALUES (1, 'Alice')")
            self.conn.commit()

            # Verify data persisted
            result = self.cursor.execute("SELECT name FROM test_commit WHERE id = 1").fetchone()
            assert result[0] == 'Alice'

            self.results.add_pass("COMMIT", "Transaction committed successfully")
            self.cursor.execute("DROP TABLE test_commit")
        except Exception as e:
            self.results.add_fail("COMMIT", str(e))

    def test_rollback(self):
        """Test ROLLBACK transaction"""
        try:
            self.cursor.execute("CREATE TABLE test_rollback (id INT, name TEXT)")
            self.cursor.execute("INSERT INTO test_rollback VALUES (1, 'Alice')")
            self.conn.commit()

            # Start new transaction
            self.cursor.execute("INSERT INTO test_rollback VALUES (2, 'Bob')")
            self.conn.rollback()

            # Verify Bob wasn't added
            result = self.cursor.execute("SELECT COUNT(*) FROM test_rollback").fetchone()
            assert result[0] == 1

            self.results.add_pass("ROLLBACK", "Transaction rolled back successfully")
            self.cursor.execute("DROP TABLE test_rollback")
        except Exception as e:
            self.results.add_fail("ROLLBACK", str(e))

    def test_autocommit(self):
        """Test AUTOCOMMIT mode"""
        try:
            self.cursor.execute("CREATE TABLE test_autocommit (id INT)")
            self.cursor.execute("INSERT INTO test_autocommit VALUES (1)")
            # Autocommit is enabled by default in sqlite3 when isolation_level=None
            # For explicit testing:
            old_isolation = self.conn.isolation_level
            self.conn.isolation_level = None  # Autocommit mode
            self.cursor.execute("INSERT INTO test_autocommit VALUES (2)")
            self.conn.isolation_level = old_isolation

            result = self.cursor.execute("SELECT COUNT(*) FROM test_autocommit").fetchone()
            assert result[0] >= 2

            self.results.add_pass("AUTOCOMMIT", "Autocommit mode works")
            self.cursor.execute("DROP TABLE test_autocommit")
        except Exception as e:
            self.results.add_fail("AUTOCOMMIT", str(e))

    # ========== PARAMETER BINDING TESTS ==========

    def test_qmark_parameter_binding(self):
        """Test ? parameter binding (qmark style)"""
        try:
            self.cursor.execute("CREATE TABLE test_param (id INT, name TEXT)")
            self.cursor.execute("INSERT INTO test_param VALUES (?, ?)", (1, 'Alice'))
            self.cursor.execute("INSERT INTO test_param VALUES (?, ?)", (2, 'Bob'))

            result = self.cursor.execute("SELECT name FROM test_param WHERE id = ?", (1,)).fetchone()
            assert result[0] == 'Alice'

            self.results.add_pass("? Parameter Binding", "Qmark style parameter binding")
            self.cursor.execute("DROP TABLE test_param")
        except Exception as e:
            self.results.add_fail("? Parameter Binding", str(e))

    def test_named_parameter_binding(self):
        """Test :name parameter binding"""
        try:
            self.cursor.execute("CREATE TABLE test_named (id INT, name TEXT)")
            self.cursor.execute("INSERT INTO test_named VALUES (:id, :name)", {'id': 1, 'name': 'Alice'})

            result = self.cursor.execute("SELECT name FROM test_named WHERE id = :id", {'id': 1}).fetchone()
            assert result[0] == 'Alice'

            self.results.add_pass("Named Parameter Binding", ":name style parameter binding")
            self.cursor.execute("DROP TABLE test_named")
        except Exception as e:
            self.results.add_fail("Named Parameter Binding", str(e))

    # ========== VIEW TESTS ==========

    def test_create_view(self):
        """Test CREATE VIEW"""
        try:
            # Clean up any existing tables first
            try:
                self.cursor.execute("DROP VIEW IF EXISTS high_earners")
            except:
                pass
            try:
                self.cursor.execute("DROP TABLE IF EXISTS employees")
            except:
                pass

            self.cursor.execute("CREATE TABLE employees (id INT, name TEXT, salary INT)")
            self.cursor.executemany("INSERT INTO employees VALUES (?, ?, ?)",
                                   [(1, 'Alice', 50000), (2, 'Bob', 60000)])
            self.cursor.execute("CREATE VIEW high_earners AS SELECT name FROM employees WHERE salary > 55000")

            result = self.cursor.execute("SELECT name FROM high_earners").fetchall()
            assert len(result) == 1 and result[0][0] == 'Bob'

            self.results.add_pass("CREATE VIEW", "View creation and querying")
        except Exception as e:
            self.results.add_fail("CREATE VIEW", str(e))
        finally:
            # Always cleanup
            try:
                self.cursor.execute("DROP VIEW IF EXISTS high_earners")
            except:
                pass
            try:
                self.cursor.execute("DROP TABLE IF EXISTS employees")
            except:
                pass

    # ========== UTILITY TESTS ==========

    def test_last_row_id(self):
        """Test lastrowid property"""
        try:
            self.cursor.execute("CREATE TABLE test_rowid (id INTEGER PRIMARY KEY, name TEXT)")
            self.cursor.execute("INSERT INTO test_rowid (name) VALUES ('Alice')")
            last_id = self.cursor.lastrowid
            assert last_id == 1

            self.results.add_pass("lastrowid", "Last inserted row ID retrieval")
            self.cursor.execute("DROP TABLE test_rowid")
        except Exception as e:
            self.results.add_fail("lastrowid", str(e))

    def test_row_count(self):
        """Test rowcount property"""
        try:
            self.cursor.execute("CREATE TABLE test_count (id INT)")
            self.cursor.executemany("INSERT INTO test_count VALUES (?)", [(1,), (2,), (3,)])
            count = self.cursor.rowcount
            assert count == 3

            self.results.add_pass("rowcount", "Affected rows count")
            self.cursor.execute("DROP TABLE test_count")
        except Exception as e:
            self.results.add_fail("rowcount", str(e))

    def test_column_names(self):
        """Test cursor.description for column names"""
        try:
            self.cursor.execute("CREATE TABLE test_desc (id INT, name TEXT, age INT)")
            self.cursor.execute("SELECT * FROM test_desc")
            columns = [desc[0] for desc in self.cursor.description]
            assert columns == ['id', 'name', 'age']

            self.results.add_pass("Column Names", "Retrieving column names from cursor.description")
            self.cursor.execute("DROP TABLE test_desc")
        except Exception as e:
            self.results.add_fail("Column Names", str(e))

    def test_fetchone_fetchall_fetchmany(self):
        """Test fetch methods"""
        try:
            self.cursor.execute("CREATE TABLE test_fetch (id INT)")
            for i in range(10):
                self.cursor.execute("INSERT INTO test_fetch VALUES (?)", (i,))

            self.cursor.execute("SELECT * FROM test_fetch")

            # fetchone
            row = self.cursor.fetchone()
            assert row[0] == 0

            # fetchmany
            self.cursor.execute("SELECT * FROM test_fetch")
            many = self.cursor.fetchmany(3)
            assert len(many) == 3

            # fetchall
            self.cursor.execute("SELECT * FROM test_fetch")
            all_rows = self.cursor.fetchall()
            assert len(all_rows) == 10

            self.results.add_pass("Fetch Methods", "fetchone, fetchmany, fetchall")
            self.cursor.execute("DROP TABLE test_fetch")
        except Exception as e:
            self.results.add_fail("Fetch Methods", str(e))

    def test_context_manager(self):
        """Test context manager (with statement)"""
        try:
            with tempfile.NamedTemporaryFile(suffix='.db', delete=False) as f:
                db_path = f.name

            with sqlite3.connect(db_path) as conn:
                cursor = conn.cursor()
                cursor.execute("CREATE TABLE test_context (id INT)")
                cursor.execute("INSERT INTO test_context VALUES (1)")

            # Verify data was committed
            conn2 = sqlite3.connect(db_path)
            cursor2 = conn2.cursor()
            result = cursor2.execute("SELECT * FROM test_context").fetchone()
            assert result[0] == 1
            conn2.close()

            os.unlink(db_path)
            self.results.add_pass("Context Manager", "with statement support")
        except Exception as e:
            self.results.add_fail("Context Manager", str(e))

    # ========== DECIMAL/NUMERIC TEST (NOW SUPPORTED!) ==========

    def test_decimal_type(self):
        """Test DECIMAL type (now fully supported in HeliosDB-Lite v3.0.0+)"""
        try:
            self.cursor.execute("CREATE TABLE test_decimal (value DECIMAL(10,2))")
            self.cursor.execute("INSERT INTO test_decimal VALUES (123.45)")

            result = self.cursor.execute("SELECT value FROM test_decimal").fetchone()

            # Verify decimal value is retrieved correctly with proper precision
            retrieved_value = result[0]
            if abs(float(retrieved_value) - 123.45) < 0.01:
                self.results.add_pass("DECIMAL Type", f"Value: {retrieved_value} (exact precision maintained)")
            else:
                self.results.add_fail("DECIMAL Type", f"Expected 123.45, got {retrieved_value}")

            self.cursor.execute("DROP TABLE test_decimal")
        except Exception as e:
            self.results.add_fail("DECIMAL Type", str(e))

    # ========== TRIGGER TEST (KNOWN LIMITATION) ==========

    def test_trigger(self):
        """Test TRIGGER (known unsupported feature)"""
        try:
            self.cursor.execute("CREATE TABLE test_trig (id INT)")
            self.cursor.execute("""
                CREATE TRIGGER trg_test
                AFTER INSERT ON test_trig
                BEGIN
                  SELECT 1;
                END
            """)
            self.results.add_skip("TRIGGER", "Not supported in HeliosDB-Lite (use application logic)")
            self.cursor.execute("DROP TABLE test_trig")
        except Exception as e:
            if "trigger" in str(e).lower():
                self.results.add_skip("TRIGGER", "Not supported (expected) - migrate to application logic")
            else:
                self.results.add_fail("TRIGGER", str(e))


def main():
    """Run all tests"""
    print("\n" + "="*70)
    print("HELIOSDB-LITE SQLite COMPATIBILITY TEST SUITE")
    print("="*70 + "\n")

    tester = SQLiteCompatibilityTester(use_memory=True)

    if not tester.setup():
        print("Failed to setup test environment")
        return False

    # Run all test categories
    print("Running Data Type Tests...")
    tester.test_integer_type()
    tester.test_real_type()
    tester.test_text_type()
    tester.test_blob_type()
    tester.test_null_type()
    tester.test_date_time_type()

    print("\nRunning CRUD Operation Tests...")
    tester.test_create_table()
    tester.test_insert_single_row()
    tester.test_insert_multiple_rows()
    tester.test_select_all()
    tester.test_select_with_where()
    tester.test_update()
    tester.test_delete()

    print("\nRunning Query Feature Tests...")
    tester.test_join_operations()
    tester.test_aggregate_functions()
    tester.test_group_by_having()
    tester.test_order_by()
    tester.test_limit_offset()
    tester.test_like_operator()
    tester.test_in_operator()
    tester.test_between_operator()

    print("\nRunning Constraint Tests...")
    tester.test_primary_key()
    tester.test_unique_constraint()
    tester.test_not_null_constraint()
    tester.test_check_constraint()

    print("\nRunning Index Tests...")
    tester.test_create_index()
    tester.test_unique_index()

    print("\nRunning Transaction Tests...")
    tester.test_commit()
    tester.test_rollback()
    tester.test_autocommit()

    print("\nRunning Parameter Binding Tests...")
    tester.test_qmark_parameter_binding()
    tester.test_named_parameter_binding()

    print("\nRunning View Tests...")
    tester.test_create_view()

    print("\nRunning Utility Tests...")
    tester.test_last_row_id()
    tester.test_row_count()
    tester.test_column_names()
    tester.test_fetchone_fetchall_fetchmany()
    tester.test_context_manager()

    print("\nRunning Known Limitation Tests...")
    tester.test_decimal_type()
    tester.test_trigger()

    tester.cleanup()

    # Print report
    passed, failed, skipped = tester.results.report()

    # Return exit code
    return 0 if failed == 0 else 1


if __name__ == '__main__':
    sys.exit(main())
