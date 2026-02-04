"""
SQLite-compatible exceptions for heliosdb-sqlite.

This module provides DB-API 2.0 compliant exception hierarchy.
All exceptions are compatible with Python's sqlite3 module.
"""

from typing import Optional


class Error(Exception):
    """Base class for all heliosdb-sqlite exceptions."""

    pass


class Warning(Exception):
    """Exception raised for important warnings."""

    pass


class InterfaceError(Error):
    """
    Exception raised for errors related to the database interface.

    Examples: incorrect parameter types, connection closed, etc.
    """

    pass


class DatabaseError(Error):
    """
    Exception raised for errors related to the database.

    This is the base class for all database-related errors.
    """

    pass


class DataError(DatabaseError):
    """
    Exception raised for errors due to problems with processed data.

    Examples: division by zero, numeric value out of range, etc.
    """

    pass


class OperationalError(DatabaseError):
    """
    Exception raised for errors related to database operation.

    Examples: unexpected disconnect, database not found, transaction
    processing error, memory allocation error, etc.
    """

    pass


class IntegrityError(DatabaseError):
    """
    Exception raised when database integrity is affected.

    Examples: foreign key constraint fails, unique constraint violation, etc.
    """

    pass


class InternalError(DatabaseError):
    """
    Exception raised for internal database errors.

    Examples: cursor not valid, transaction out of sync, etc.
    """

    pass


class ProgrammingError(DatabaseError):
    """
    Exception raised for programming errors.

    Examples: table not found, syntax error in SQL statement,
    wrong number of parameters, etc.
    """

    pass


class NotSupportedError(DatabaseError):
    """
    Exception raised for use of unsupported database features.

    Examples: calling rollback() on a connection that doesn't support
    transactions, using unsupported SQL syntax, etc.
    """

    pass
