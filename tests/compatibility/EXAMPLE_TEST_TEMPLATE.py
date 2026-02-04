#!/usr/bin/env python3
"""
Example Test Template for HeliosDB SQLite Compatibility Tests

This file demonstrates best practices for writing new tests.
Use this as a template when adding new test cases.
"""

import pytest
import sqlite3
import tempfile
import os
from typing import Any, Callable


# ============================================================================
# Example: Basic Test Structure
# ============================================================================

class TestExampleBasicStructure:
    """
    Example test class demonstrating basic test structure.

    Test classes should:
    - Be prefixed with 'Test'
    - Have descriptive docstrings
    - Group related tests together
    """

    def test_example_simple(self, memory_connection):
        """
        Example simple test with Arrange-Act-Assert pattern.

        This test demonstrates:
        - Using fixtures (memory_connection)
        - Clear test structure
        - Single assertion focus
        """
        # Arrange - Set up test data
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE users (id INTEGER, name TEXT)")

        # Act - Perform the operation being tested
        cursor.execute("INSERT INTO users VALUES (1, 'Alice')")

        # Assert - Verify the expected outcome
        cursor.execute("SELECT * FROM users")
        result = cursor.fetchone()
        assert result == (1, 'Alice'), "Expected user record to match"


# ============================================================================
# Example: Using Fixtures
# ============================================================================

@pytest.fixture
def sample_data():
    """
    Example custom fixture providing sample test data.

    Fixtures can:
    - Provide test data
    - Set up complex state
    - Share setup across tests
    """
    return [
        (1, "Alice", 30),
        (2, "Bob", 25),
        (3, "Charlie", 35),
    ]


class TestExampleFixtures:
    """Example tests using fixtures"""

    def test_with_memory_connection(self, memory_connection):
        """Example using memory_connection fixture"""
        cursor = memory_connection.cursor()
        cursor.execute("SELECT 1")
        assert cursor.fetchone() == (1,)

    def test_with_custom_fixture(self, memory_connection, sample_data):
        """Example using custom fixture"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER)")

        # Use sample data from fixture
        cursor.executemany(
            "INSERT INTO users VALUES (?, ?, ?)",
            sample_data
        )

        cursor.execute("SELECT COUNT(*) FROM users")
        assert cursor.fetchone()[0] == len(sample_data)


# ============================================================================
# Example: Parametrized Tests
# ============================================================================

class TestExampleParametrized:
    """Example parametrized tests for testing multiple scenarios"""

    @pytest.mark.parametrize("value,expected", [
        (1, int),
        (3.14, float),
        ("hello", str),
        (b"bytes", bytes),
        (None, type(None)),
    ])
    def test_data_type_preservation(self, memory_connection, value, expected):
        """
        Example parametrized test for data type preservation.

        This single test runs multiple times with different parameters.
        """
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (val)")
        cursor.execute("INSERT INTO test VALUES (?)", (value,))
        cursor.execute("SELECT val FROM test")

        result = cursor.fetchone()[0]
        assert type(result) == expected, f"Expected {expected}, got {type(result)}"


# ============================================================================
# Example: Test Markers
# ============================================================================

class TestExampleMarkers:
    """Example tests using pytest markers"""

    @pytest.mark.slow
    def test_large_dataset(self, memory_connection):
        """
        Example test marked as 'slow'.

        Run with: pytest -m slow
        Skip with: pytest -m "not slow"
        """
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER)")

        # Insert large dataset
        data = [(i,) for i in range(10000)]
        cursor.executemany("INSERT INTO test VALUES (?)", data)

        cursor.execute("SELECT COUNT(*) FROM test")
        assert cursor.fetchone()[0] == 10000

    @pytest.mark.requires_server
    def test_server_feature(self, heliosdb_server):
        """
        Example test requiring HeliosDB server.

        This test only runs when server fixture is available.
        """
        config = heliosdb_server
        assert config["port"] > 0

    @pytest.mark.skip(reason="Feature not implemented yet")
    def test_future_feature(self):
        """Example skipped test for future functionality"""
        pass

    @pytest.mark.xfail(reason="Known issue #123")
    def test_known_issue(self):
        """Example test expected to fail (known bug)"""
        assert False, "This is a known issue"


# ============================================================================
# Example: Error Testing
# ============================================================================

class TestExampleErrorHandling:
    """Example tests for error conditions"""

    def test_syntax_error_raises_exception(self, memory_connection):
        """Example testing that errors are raised correctly"""
        cursor = memory_connection.cursor()

        with pytest.raises(sqlite3.OperationalError):
            cursor.execute("INVALID SQL SYNTAX")

    def test_constraint_violation(self, memory_connection):
        """Example testing constraint violations"""
        cursor = memory_connection.cursor()
        cursor.execute("CREATE TABLE test (id INTEGER UNIQUE)")

        cursor.execute("INSERT INTO test VALUES (1)")

        # Should raise IntegrityError on duplicate
        with pytest.raises(sqlite3.IntegrityError) as exc_info:
            cursor.execute("INSERT INTO test VALUES (1)")

        # Can also check error message
        assert "UNIQUE" in str(exc_info.value)


# ============================================================================
# Example: Setup and Teardown
# ============================================================================

class TestExampleSetupTeardown:
    """Example class-level setup and teardown"""

    @classmethod
    def setup_class(cls):
        """
        Run once before all tests in this class.

        Use for expensive setup shared across all tests.
        """
        cls.shared_data = {"initialized": True}

    @classmethod
    def teardown_class(cls):
        """
        Run once after all tests in this class.

        Use for cleanup of class-level resources.
        """
        cls.shared_data = None

    def setup_method(self):
        """
        Run before each test method.

        Use for per-test initialization.
        """
        self.test_counter = 0

    def teardown_method(self):
        """
        Run after each test method.

        Use for per-test cleanup.
        """
        self.test_counter = None

    def test_with_setup(self):
        """Example test using setup/teardown"""
        assert self.shared_data["initialized"] is True
        assert self.test_counter == 0
        self.test_counter += 1


# ============================================================================
# Example: Complex Fixture with Cleanup
# ============================================================================

@pytest.fixture
def temp_database():
    """
    Example fixture creating temporary database with cleanup.

    Uses yield for cleanup:
    - Code before yield: setup
    - yield: provide fixture value
    - Code after yield: cleanup (always runs)
    """
    # Setup
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)

    conn = sqlite3.connect(path)
    cursor = conn.cursor()
    cursor.execute("CREATE TABLE test (id INTEGER)")
    conn.commit()

    # Provide fixture
    yield {"connection": conn, "path": path}

    # Cleanup (always runs, even if test fails)
    conn.close()
    if os.path.exists(path):
        os.unlink(path)


class TestExampleComplexFixture:
    """Example using complex fixture with cleanup"""

    def test_with_temp_database(self, temp_database):
        """Example using temporary database fixture"""
        conn = temp_database["connection"]
        cursor = conn.cursor()

        cursor.execute("INSERT INTO test VALUES (1)")
        cursor.execute("SELECT * FROM test")

        assert cursor.fetchone() == (1,)

        # No need to manually clean up - fixture handles it


# ============================================================================
# Example: Helper Functions
# ============================================================================

def create_test_table(cursor, table_name: str, schema: str):
    """
    Example helper function for common operations.

    Helper functions:
    - Reduce code duplication
    - Make tests more readable
    - Can be reused across test classes
    """
    cursor.execute(f"CREATE TABLE {table_name} ({schema})")


def insert_test_data(cursor, table_name: str, data: list):
    """Example helper for inserting test data"""
    placeholders = ", ".join(["?" for _ in data[0]])
    cursor.executemany(
        f"INSERT INTO {table_name} VALUES ({placeholders})",
        data
    )


class TestExampleHelpers:
    """Example tests using helper functions"""

    def test_with_helpers(self, memory_connection):
        """Example using helper functions"""
        cursor = memory_connection.cursor()

        # Use helpers for common operations
        create_test_table(cursor, "users", "id INTEGER, name TEXT")
        insert_test_data(cursor, "users", [(1, "Alice"), (2, "Bob")])

        cursor.execute("SELECT COUNT(*) FROM users")
        assert cursor.fetchone()[0] == 2


# ============================================================================
# Example: Best Practices Summary
# ============================================================================

"""
BEST PRACTICES FOR WRITING TESTS:

1. Test Naming
   - Use descriptive names: test_what_when_then
   - Example: test_insert_returns_rowid_when_successful

2. Test Structure
   - Follow Arrange-Act-Assert pattern
   - Keep tests focused and simple
   - One logical assertion per test

3. Fixtures
   - Use fixtures for common setup
   - Clean up resources in teardown
   - Use yield for guaranteed cleanup

4. Parametrization
   - Use @pytest.mark.parametrize for multiple scenarios
   - Reduces code duplication
   - Makes test coverage clear

5. Error Testing
   - Use pytest.raises() for exception testing
   - Verify error messages when relevant
   - Test both success and failure paths

6. Markers
   - Mark slow tests: @pytest.mark.slow
   - Mark integration tests: @pytest.mark.integration
   - Skip or xfail when appropriate

7. Documentation
   - Write clear docstrings
   - Explain what is being tested
   - Document any non-obvious setup

8. Independence
   - Tests should not depend on each other
   - Each test should be able to run standalone
   - Use fixtures to share setup, not test order

9. Readability
   - Use helper functions for common operations
   - Keep test code clean and simple
   - Add comments for complex scenarios

10. Performance
    - Mark slow tests appropriately
    - Use appropriate fixtures (class vs function scope)
    - Consider parallel execution for independent tests
"""


# ============================================================================
# Running This Example
# ============================================================================

"""
Run this example file:

    # Run all examples
    pytest EXAMPLE_TEST_TEMPLATE.py -v

    # Run specific class
    pytest EXAMPLE_TEST_TEMPLATE.py::TestExampleBasicStructure -v

    # Run with markers
    pytest EXAMPLE_TEST_TEMPLATE.py -m slow -v

    # Run with verbose output
    pytest EXAMPLE_TEST_TEMPLATE.py -vv

    # Run and show print statements
    pytest EXAMPLE_TEST_TEMPLATE.py -s

    # Generate coverage
    pytest EXAMPLE_TEST_TEMPLATE.py --cov=. --cov-report=html
"""

if __name__ == "__main__":
    pytest.main([__file__, "-v"])
