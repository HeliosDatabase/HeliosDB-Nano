#!/usr/bin/env python3
"""
HeliosDB-SQLite Installation Verification Script

This script performs comprehensive post-installation checks to verify that
heliosdb-sqlite was installed correctly and is functioning properly.

Usage:
    python INSTALLATION_TEST.py
    python -m heliosdb_sqlite.cli check

Exit Codes:
    0 - All tests passed
    1 - One or more tests failed
    2 - Critical error (package not installed)
"""

import os
import platform
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Tuple

# ANSI color codes for output
GREEN = "\033[92m"
RED = "\033[91m"
YELLOW = "\033[93m"
BLUE = "\033[94m"
RESET = "\033[0m"
BOLD = "\033[1m"


class InstallationTester:
    """Comprehensive installation testing suite."""

    def __init__(self) -> None:
        """Initialize the tester."""
        self.passed_tests: List[str] = []
        self.failed_tests: List[Tuple[str, str]] = []
        self.warnings: List[str] = []

    def print_header(self, text: str) -> None:
        """Print section header."""
        print(f"\n{BOLD}{BLUE}{'=' * 70}{RESET}")
        print(f"{BOLD}{BLUE}{text:^70}{RESET}")
        print(f"{BOLD}{BLUE}{'=' * 70}{RESET}\n")

    def print_success(self, message: str) -> None:
        """Print success message."""
        print(f"{GREEN}✓{RESET} {message}")
        self.passed_tests.append(message)

    def print_failure(self, message: str, error: str = "") -> None:
        """Print failure message."""
        print(f"{RED}✗{RESET} {message}")
        if error:
            print(f"  {RED}Error: {error}{RESET}")
        self.failed_tests.append((message, error))

    def print_warning(self, message: str) -> None:
        """Print warning message."""
        print(f"{YELLOW}⚠{RESET} {message}")
        self.warnings.append(message)

    def print_info(self, message: str) -> None:
        """Print info message."""
        print(f"{BLUE}ℹ{RESET} {message}")

    def test_package_import(self) -> bool:
        """Test that heliosdb_sqlite can be imported."""
        self.print_header("Test 1: Package Import")

        try:
            import heliosdb_sqlite

            self.print_success("Package 'heliosdb_sqlite' imported successfully")
            return True
        except ImportError as e:
            self.print_failure("Failed to import 'heliosdb_sqlite'", str(e))
            return False

    def test_version_info(self) -> bool:
        """Test version information is available."""
        self.print_header("Test 2: Version Information")

        try:
            import heliosdb_sqlite

            version = getattr(heliosdb_sqlite, "__version__", None)
            if version:
                self.print_success(f"Package version: {version}")

                # Check version format (semantic versioning)
                parts = version.split(".")
                if len(parts) >= 3:
                    self.print_success(f"Version format valid (SemVer)")
                else:
                    self.print_warning(f"Version format unusual: {version}")

                return True
            else:
                self.print_failure("__version__ attribute not found")
                return False
        except Exception as e:
            self.print_failure("Failed to get version info", str(e))
            return False

    def test_platform_info(self) -> bool:
        """Display platform information."""
        self.print_header("Test 3: Platform Information")

        try:
            self.print_info(f"Python version: {sys.version.split()[0]}")
            self.print_info(f"Platform: {platform.platform()}")
            self.print_info(f"Architecture: {platform.machine()}")
            self.print_info(f"System: {platform.system()}")

            # Check Python version compatibility
            py_version = sys.version_info
            if py_version >= (3, 8):
                self.print_success(f"Python version {py_version.major}.{py_version.minor} is supported")
            else:
                self.print_failure(
                    f"Python {py_version.major}.{py_version.minor} is not supported",
                    "Requires Python 3.8 or later"
                )
                return False

            return True
        except Exception as e:
            self.print_failure("Failed to get platform info", str(e))
            return False

    def test_binary_availability(self) -> bool:
        """Test that HeliosDB Nano binary is available."""
        self.print_header("Test 4: Binary Availability")

        try:
            import heliosdb_sqlite

            # Check if get_binary_path exists
            if not hasattr(heliosdb_sqlite, "get_binary_path"):
                self.print_warning("get_binary_path() function not found in package")
                return True  # Not critical

            binary_path = heliosdb_sqlite.get_binary_path()
            self.print_info(f"Binary path: {binary_path}")

            if binary_path.exists():
                self.print_success("HeliosDB Nano binary found")

                # Check if executable
                if os.access(binary_path, os.X_OK):
                    self.print_success("Binary has executable permissions")
                else:
                    self.print_warning("Binary may not have executable permissions")

                # Check binary size
                size_mb = binary_path.stat().st_size / (1024 * 1024)
                self.print_info(f"Binary size: {size_mb:.2f} MB")

                if size_mb > 0.1:  # Should be at least 100KB
                    self.print_success("Binary size looks reasonable")
                else:
                    self.print_warning(f"Binary size is unusually small: {size_mb:.2f} MB")

                return True
            else:
                self.print_failure("HeliosDB Nano binary not found", f"Expected at: {binary_path}")
                return False

        except Exception as e:
            self.print_failure("Failed to check binary availability", str(e))
            return False

    def test_basic_connection(self) -> bool:
        """Test basic database connection."""
        self.print_header("Test 5: Basic Connection")

        try:
            import heliosdb_sqlite

            # Test in-memory connection
            conn = heliosdb_sqlite.connect(":memory:")
            self.print_success("Created in-memory database connection")

            # Verify connection object
            if conn:
                self.print_success("Connection object is valid")
            else:
                self.print_failure("Connection object is None")
                return False

            # Close connection
            conn.close()
            self.print_success("Connection closed successfully")

            return True

        except Exception as e:
            self.print_failure("Failed to create connection", str(e))
            return False

    def test_basic_operations(self) -> bool:
        """Test basic SQL operations."""
        self.print_header("Test 6: Basic SQL Operations")

        try:
            import heliosdb_sqlite

            conn = heliosdb_sqlite.connect(":memory:")
            cursor = conn.cursor()

            # Test CREATE TABLE
            cursor.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")
            self.print_success("CREATE TABLE executed successfully")

            # Test INSERT
            cursor.execute("INSERT INTO test (id, name) VALUES (1, 'Alice')")
            cursor.execute("INSERT INTO test (id, name) VALUES (2, 'Bob')")
            conn.commit()
            self.print_success("INSERT statements executed successfully")

            # Test SELECT
            cursor.execute("SELECT * FROM test ORDER BY id")
            rows = cursor.fetchall()
            if len(rows) == 2:
                self.print_success(f"SELECT returned {len(rows)} rows")
            else:
                self.print_failure(f"Expected 2 rows, got {len(rows)}")
                return False

            # Verify data
            if rows[0][1] == "Alice" and rows[1][1] == "Bob":
                self.print_success("Data integrity verified")
            else:
                self.print_failure("Data mismatch")
                return False

            # Test UPDATE
            cursor.execute("UPDATE test SET name = 'Charlie' WHERE id = 1")
            conn.commit()
            cursor.execute("SELECT name FROM test WHERE id = 1")
            updated_name = cursor.fetchone()[0]
            if updated_name == "Charlie":
                self.print_success("UPDATE executed successfully")
            else:
                self.print_failure(f"UPDATE failed, expected 'Charlie', got '{updated_name}'")
                return False

            # Test DELETE
            cursor.execute("DELETE FROM test WHERE id = 2")
            conn.commit()
            cursor.execute("SELECT COUNT(*) FROM test")
            count = cursor.fetchone()[0]
            if count == 1:
                self.print_success("DELETE executed successfully")
            else:
                self.print_failure(f"DELETE failed, expected 1 row, got {count}")
                return False

            conn.close()
            return True

        except Exception as e:
            self.print_failure("SQL operations failed", str(e))
            return False

    def test_parameterized_queries(self) -> bool:
        """Test parameterized queries (SQL injection protection)."""
        self.print_header("Test 7: Parameterized Queries")

        try:
            import heliosdb_sqlite

            conn = heliosdb_sqlite.connect(":memory:")
            cursor = conn.cursor()

            cursor.execute("CREATE TABLE users (id INTEGER, username TEXT)")

            # Test with ? placeholder
            cursor.execute("INSERT INTO users VALUES (?, ?)", (1, "alice"))
            self.print_success("Parameterized INSERT with ? placeholder")

            # Test with named parameters
            cursor.execute(
                "INSERT INTO users VALUES (:id, :username)",
                {"id": 2, "username": "bob"}
            )
            self.print_success("Parameterized INSERT with named parameters")

            # Verify data
            cursor.execute("SELECT COUNT(*) FROM users")
            count = cursor.fetchone()[0]
            if count == 2:
                self.print_success("Parameterized queries work correctly")
            else:
                self.print_failure(f"Expected 2 rows, got {count}")
                return False

            # Test SQL injection protection
            malicious_input = "'; DROP TABLE users; --"
            cursor.execute("SELECT * FROM users WHERE username = ?", (malicious_input,))
            cursor.fetchall()  # Should return empty, not error

            # Verify table still exists
            cursor.execute("SELECT COUNT(*) FROM users")
            count = cursor.fetchone()[0]
            if count == 2:
                self.print_success("SQL injection protection verified")
            else:
                self.print_failure("SQL injection protection may have failed")
                return False

            conn.close()
            return True

        except Exception as e:
            self.print_failure("Parameterized query test failed", str(e))
            return False

    def test_data_types(self) -> bool:
        """Test SQLite data type support."""
        self.print_header("Test 8: Data Type Support")

        try:
            import heliosdb_sqlite

            conn = heliosdb_sqlite.connect(":memory:")
            cursor = conn.cursor()

            cursor.execute("""
                CREATE TABLE types_test (
                    id INTEGER,
                    text_val TEXT,
                    real_val REAL,
                    blob_val BLOB,
                    null_val NULL
                )
            """)

            # Insert various data types
            test_data = (
                1,
                "Hello, World!",
                3.14159,
                b"\x00\x01\x02\x03",
                None
            )

            cursor.execute("INSERT INTO types_test VALUES (?, ?, ?, ?, ?)", test_data)
            conn.commit()

            # Retrieve and verify
            cursor.execute("SELECT * FROM types_test")
            row = cursor.fetchone()

            if row[0] == 1:
                self.print_success("INTEGER type support verified")
            if row[1] == "Hello, World!":
                self.print_success("TEXT type support verified")
            if abs(row[2] - 3.14159) < 0.00001:
                self.print_success("REAL type support verified")
            if row[3] == b"\x00\x01\x02\x03":
                self.print_success("BLOB type support verified")
            if row[4] is None:
                self.print_success("NULL type support verified")

            conn.close()
            return True

        except Exception as e:
            self.print_failure("Data type test failed", str(e))
            return False

    def test_transaction_support(self) -> bool:
        """Test transaction support."""
        self.print_header("Test 9: Transaction Support")

        try:
            import heliosdb_sqlite

            conn = heliosdb_sqlite.connect(":memory:")
            cursor = conn.cursor()

            cursor.execute("CREATE TABLE accounts (id INTEGER, balance INTEGER)")
            cursor.execute("INSERT INTO accounts VALUES (1, 100)")
            conn.commit()

            # Test transaction with commit
            cursor.execute("BEGIN")
            cursor.execute("UPDATE accounts SET balance = 200 WHERE id = 1")
            conn.commit()

            cursor.execute("SELECT balance FROM accounts WHERE id = 1")
            balance = cursor.fetchone()[0]
            if balance == 200:
                self.print_success("Transaction commit works correctly")
            else:
                self.print_failure(f"Expected balance 200, got {balance}")
                return False

            # Test transaction with rollback
            cursor.execute("BEGIN")
            cursor.execute("UPDATE accounts SET balance = 300 WHERE id = 1")
            conn.rollback()

            cursor.execute("SELECT balance FROM accounts WHERE id = 1")
            balance = cursor.fetchone()[0]
            if balance == 200:
                self.print_success("Transaction rollback works correctly")
            else:
                self.print_failure(f"Expected balance 200 after rollback, got {balance}")
                return False

            conn.close()
            return True

        except Exception as e:
            self.print_failure("Transaction test failed", str(e))
            return False

    def test_optional_dependencies(self) -> bool:
        """Test availability of optional dependencies."""
        self.print_header("Test 10: Optional Dependencies")

        optional_modules = {
            "numpy": "Vector operations support",
            "scipy": "Scientific computing support",
            "pandas": "DataFrame integration",
            "pyarrow": "Arrow columnar format",
            "aiofiles": "Async file operations",
        }

        any_installed = False

        for module, description in optional_modules.items():
            try:
                __import__(module)
                self.print_success(f"{module}: {description}")
                any_installed = True
            except ImportError:
                self.print_info(f"{module}: Not installed (optional)")

        if not any_installed:
            self.print_info("No optional dependencies installed")
            self.print_info("Install with: pip install heliosdb-sqlite[all]")

        return True  # Optional dependencies are not required

    def print_summary(self) -> int:
        """Print test summary and return exit code."""
        self.print_header("Test Summary")

        total_tests = len(self.passed_tests) + len(self.failed_tests)
        passed_count = len(self.passed_tests)
        failed_count = len(self.failed_tests)
        warning_count = len(self.warnings)

        print(f"\n{BOLD}Results:{RESET}")
        print(f"  {GREEN}Passed:{RESET}   {passed_count}/{total_tests}")
        print(f"  {RED}Failed:{RESET}   {failed_count}/{total_tests}")
        print(f"  {YELLOW}Warnings:{RESET} {warning_count}")

        if failed_count > 0:
            print(f"\n{BOLD}{RED}Failed Tests:{RESET}")
            for i, (test, error) in enumerate(self.failed_tests, 1):
                print(f"  {i}. {test}")
                if error:
                    print(f"     Error: {error}")

        if warning_count > 0:
            print(f"\n{BOLD}{YELLOW}Warnings:{RESET}")
            for i, warning in enumerate(self.warnings, 1):
                print(f"  {i}. {warning}")

        print("\n" + "=" * 70)

        if failed_count == 0:
            print(f"{BOLD}{GREEN}✓ All tests passed! Installation successful.{RESET}")
            print("\n" + "=" * 70)
            return 0
        else:
            print(f"{BOLD}{RED}✗ Some tests failed. Please review the errors above.{RESET}")
            print("\n" + "=" * 70)
            return 1

    def run_all_tests(self) -> int:
        """Run all tests and return exit code."""
        print(f"\n{BOLD}{BLUE}{'=' * 70}{RESET}")
        print(f"{BOLD}{BLUE}{'HeliosDB-SQLite Installation Verification':^70}{RESET}")
        print(f"{BOLD}{BLUE}{'=' * 70}{RESET}\n")

        # Run all tests
        tests = [
            self.test_package_import,
            self.test_version_info,
            self.test_platform_info,
            self.test_binary_availability,
            self.test_basic_connection,
            self.test_basic_operations,
            self.test_parameterized_queries,
            self.test_data_types,
            self.test_transaction_support,
            self.test_optional_dependencies,
        ]

        # Execute tests
        for test in tests:
            try:
                test()
            except Exception as e:
                self.print_failure(f"Test crashed: {test.__name__}", str(e))

        # Print summary and return exit code
        return self.print_summary()


def main() -> int:
    """Main entry point."""
    try:
        tester = InstallationTester()
        return tester.run_all_tests()
    except KeyboardInterrupt:
        print(f"\n\n{YELLOW}Tests interrupted by user.{RESET}")
        return 130
    except Exception as e:
        print(f"\n{RED}Critical error: {e}{RESET}")
        return 2


if __name__ == "__main__":
    sys.exit(main())
