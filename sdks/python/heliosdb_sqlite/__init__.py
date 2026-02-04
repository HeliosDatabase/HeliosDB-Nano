"""
HeliosDB SQLite Compatibility Layer

Drop-in replacement for Python's sqlite3 module that routes all operations
to HeliosDB-Lite while maintaining 100% API compatibility.

Usage:
    # Instead of:
    # import sqlite3

    # Use:
    import heliosdb_sqlite as sqlite3

    # Rest of code remains unchanged
    conn = sqlite3.connect('mydb.db')
    cursor = conn.cursor()
    cursor.execute("SELECT * FROM users")
"""

__version__ = "3.0.1"
__author__ = "HeliosDB Team"

from .main import (
    # Core classes
    Connection,
    Cursor,
    Row,

    # Functions
    connect,
    register_adapter,
    register_converter,

    # Exceptions
    Error,
    Warning,
    DatabaseError,
    IntegrityError,
    ProgrammingError,
    OperationalError,
    NotSupportedError,
    InterfaceError,
    InternalError,
    DataError,

    # Constants
    PARSE_DECLTYPES,
    PARSE_COLNAMES,
    SQLITE_OK,
    SQLITE_ERROR,
    SQLITE_DENY,
    SQLITE_IGNORE,

    # Type converters
    Binary,
    Date,
    Time,
    Timestamp,
    DateFromTicks,
    TimeFromTicks,
    TimestampFromTicks,

    # Version info
    sqlite_version,
    sqlite_version_info,
    version,
    version_info,

    # Advanced features
    enable_callback_tracebacks,
    complete_statement,
    register_trace_callback,
)

__all__ = [
    'Connection',
    'Cursor',
    'Row',
    'connect',
    'register_adapter',
    'register_converter',
    'Error',
    'Warning',
    'DatabaseError',
    'IntegrityError',
    'ProgrammingError',
    'OperationalError',
    'NotSupportedError',
    'InterfaceError',
    'InternalError',
    'DataError',
    'PARSE_DECLTYPES',
    'PARSE_COLNAMES',
    'SQLITE_OK',
    'SQLITE_ERROR',
    'SQLITE_DENY',
    'SQLITE_IGNORE',
    'Binary',
    'Date',
    'Time',
    'Timestamp',
    'DateFromTicks',
    'TimeFromTicks',
    'TimestampFromTicks',
    'sqlite_version',
    'sqlite_version_info',
    'version',
    'version_info',
    'enable_callback_tracebacks',
    'complete_statement',
    'register_trace_callback',
]
