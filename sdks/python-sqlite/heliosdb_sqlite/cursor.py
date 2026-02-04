"""
SQLite-compatible Cursor class for heliosdb-sqlite.

This module provides the Cursor class that implements the DB-API 2.0
cursor interface with full SQLite compatibility.
"""

from typing import Any, List, Optional, Sequence, Tuple

from .exceptions import InterfaceError, ProgrammingError


class Cursor:
    """
    Database cursor for executing queries and fetching results.

    This class is compatible with Python's sqlite3.Cursor interface
    and can be used as a drop-in replacement.

    Example:
        >>> cursor = connection.cursor()
        >>> cursor.execute('SELECT * FROM users WHERE id = ?', (1,))
        >>> row = cursor.fetchone()
        >>> print(row)
    """

    def __init__(self, connection: Any) -> None:
        """
        Initialize a cursor.

        Args:
            connection: Parent Connection object
        """
        self.connection = connection
        self._description: Optional[Sequence[Tuple]] = None
        self._rowcount: int = -1
        self._arraysize: int = 1
        self._closed = False
        self._results: List[Any] = []
        self._result_index = 0

    @property
    def description(self) -> Optional[Sequence[Tuple]]:
        """
        Get description of result columns.

        Returns a sequence of 7-item sequences. Each sequence contains
        information describing one result column:
        (name, type_code, display_size, internal_size, precision, scale, null_ok)

        Returns:
            Optional[Sequence[Tuple]]: Column descriptions, or None if no results
        """
        return self._description

    @property
    def rowcount(self) -> int:
        """
        Get number of rows affected by last operation.

        For SELECT statements, this is -1 (use fetchall() for count).
        For INSERT, UPDATE, DELETE, this is the number of rows affected.

        Returns:
            int: Number of rows affected, or -1 if not applicable
        """
        return self._rowcount

    @property
    def arraysize(self) -> int:
        """
        Get/set default number of rows for fetchmany().

        Default is 1, meaning fetch a single row at a time.

        Returns:
            int: Number of rows to fetch
        """
        return self._arraysize

    @arraysize.setter
    def arraysize(self, size: int) -> None:
        """Set arraysize."""
        self._arraysize = size

    @property
    def lastrowid(self) -> Optional[int]:
        """
        Get row ID of last inserted row.

        Returns:
            Optional[int]: Last row ID, or None if not applicable
        """
        # TODO: Implement lastrowid tracking
        return None

    def execute(self, sql: str, parameters: Sequence[Any] = ()) -> "Cursor":
        """
        Execute a SQL statement.

        Args:
            sql: SQL statement to execute
            parameters: Parameters for placeholders (? or :name)

        Returns:
            Cursor: Self, for method chaining

        Raises:
            InterfaceError: If cursor is closed
            ProgrammingError: If SQL is invalid

        Example:
            >>> cursor.execute('INSERT INTO users VALUES (?, ?)', (1, 'Alice'))
            >>> cursor.execute('SELECT * FROM users WHERE name = :name', {'name': 'Alice'})
        """
        if self._closed:
            raise InterfaceError("Cannot operate on a closed cursor")

        # TODO: Implement actual SQL execution
        self._description = None
        self._rowcount = -1
        self._results = []
        self._result_index = 0

        return self

    def executemany(
        self, sql: str, seq_of_parameters: Sequence[Sequence[Any]]
    ) -> "Cursor":
        """
        Execute SQL statement with multiple parameter sets.

        This is more efficient than calling execute() multiple times.

        Args:
            sql: SQL statement to execute
            seq_of_parameters: Sequence of parameter tuples

        Returns:
            Cursor: Self, for method chaining

        Example:
            >>> cursor.executemany(
            ...     'INSERT INTO users VALUES (?, ?)',
            ...     [(1, 'Alice'), (2, 'Bob'), (3, 'Charlie')]
            ... )
        """
        if self._closed:
            raise InterfaceError("Cannot operate on a closed cursor")

        # TODO: Implement batch execution
        total_rows = 0
        for parameters in seq_of_parameters:
            self.execute(sql, parameters)
            if self._rowcount >= 0:
                total_rows += self._rowcount

        self._rowcount = total_rows
        return self

    def executescript(self, sql_script: str) -> "Cursor":
        """
        Execute multiple SQL statements separated by semicolons.

        This is useful for executing initialization scripts or DDL.
        Transactions are not automatically handled.

        Args:
            sql_script: SQL script with multiple statements

        Returns:
            Cursor: Self, for method chaining

        Example:
            >>> cursor.executescript('''
            ...     CREATE TABLE users (id INTEGER, name TEXT);
            ...     CREATE TABLE orders (id INTEGER, user_id INTEGER);
            ... ''')
        """
        if self._closed:
            raise InterfaceError("Cannot operate on a closed cursor")

        # TODO: Implement script execution
        # Split by semicolons and execute each statement
        statements = [s.strip() for s in sql_script.split(";") if s.strip()]
        for statement in statements:
            self.execute(statement)

        return self

    def fetchone(self) -> Optional[Tuple]:
        """
        Fetch next row of query result.

        Returns:
            Optional[Tuple]: Next row as tuple, or None if no more rows

        Example:
            >>> cursor.execute('SELECT * FROM users')
            >>> row = cursor.fetchone()
            >>> print(row)
            (1, 'Alice')
        """
        if self._closed:
            raise InterfaceError("Cannot operate on a closed cursor")

        # TODO: Implement actual row fetching
        if self._result_index < len(self._results):
            row = self._results[self._result_index]
            self._result_index += 1
            return row
        return None

    def fetchmany(self, size: Optional[int] = None) -> List[Tuple]:
        """
        Fetch multiple rows of query result.

        Args:
            size: Number of rows to fetch (default: arraysize)

        Returns:
            List[Tuple]: List of rows (may be empty)

        Example:
            >>> cursor.execute('SELECT * FROM users')
            >>> rows = cursor.fetchmany(10)
            >>> for row in rows:
            ...     print(row)
        """
        if self._closed:
            raise InterfaceError("Cannot operate on a closed cursor")

        if size is None:
            size = self._arraysize

        results = []
        for _ in range(size):
            row = self.fetchone()
            if row is None:
                break
            results.append(row)

        return results

    def fetchall(self) -> List[Tuple]:
        """
        Fetch all remaining rows of query result.

        Returns:
            List[Tuple]: List of all remaining rows (may be empty)

        Example:
            >>> cursor.execute('SELECT * FROM users')
            >>> rows = cursor.fetchall()
            >>> print(f"Found {len(rows)} users")
        """
        if self._closed:
            raise InterfaceError("Cannot operate on a closed cursor")

        results = []
        while True:
            row = self.fetchone()
            if row is None:
                break
            results.append(row)

        return results

    def close(self) -> None:
        """
        Close the cursor.

        Attempting to use the cursor after closing will raise an InterfaceError.
        """
        self._closed = True
        self._results = []
        self._result_index = 0

    def __iter__(self) -> "Cursor":
        """Make cursor iterable."""
        return self

    def __next__(self) -> Tuple:
        """Get next row (for iteration)."""
        row = self.fetchone()
        if row is None:
            raise StopIteration
        return row
