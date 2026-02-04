"""
HeliosDB-SQLite: SQLite-compatible interface for HeliosDB-Lite

This package provides a drop-in replacement for Python's sqlite3 module,
offering enhanced features including:

- Vector search with Product Quantization
- Transparent data encryption (AES-256-GCM)
- Time-travel queries (AS OF TIMESTAMP)
- Database branching (git-like workflows)
- PostgreSQL-compatible types
- 100% SQLite API compatibility

Example:
    Basic usage (drop-in replacement for sqlite3):

    >>> import heliosdb_sqlite as sqlite3
    >>> conn = sqlite3.connect(':memory:')
    >>> cursor = conn.cursor()
    >>> cursor.execute('CREATE TABLE users (id INTEGER, name TEXT)')
    >>> cursor.execute('INSERT INTO users VALUES (?, ?)', (1, 'Alice'))
    >>> conn.commit()
    >>> cursor.execute('SELECT * FROM users')
    >>> print(cursor.fetchall())
    [(1, 'Alice')]
    >>> conn.close()

For full documentation, see: https://docs.heliosdb.io/sqlite-compat
"""

from ._version import __version__
from .connection import Connection, connect
from .cursor import Cursor
from .exceptions import (
    DatabaseError,
    DataError,
    Error,
    IntegrityError,
    InterfaceError,
    InternalError,
    NotSupportedError,
    OperationalError,
    ProgrammingError,
    Warning,
)
from .utils import get_binary_path

# SQLite API compatibility
PARSE_COLNAMES = 1
PARSE_DECLTYPES = 2
SQLITE_OK = 0
SQLITE_DENY = 1
SQLITE_IGNORE = 2

# Thread safety level (1 = serialized)
threadsafety = 1

# DB-API 2.0 compliance
apilevel = "2.0"
paramstyle = "qmark"  # Support both ? and named parameters

# Version information
version_info = tuple(map(int, __version__.split(".")))
sqlite_version = "3.43.0"  # SQLite API compatibility version
sqlite_version_info = (3, 43, 0)

__all__ = [
    # Core API
    "connect",
    "Connection",
    "Cursor",
    # Exceptions
    "Error",
    "Warning",
    "InterfaceError",
    "DatabaseError",
    "DataError",
    "OperationalError",
    "IntegrityError",
    "InternalError",
    "ProgrammingError",
    "NotSupportedError",
    # Utilities
    "get_binary_path",
    # Constants
    "PARSE_COLNAMES",
    "PARSE_DECLTYPES",
    "SQLITE_OK",
    "SQLITE_DENY",
    "SQLITE_IGNORE",
    "apilevel",
    "threadsafety",
    "paramstyle",
    "version_info",
    "sqlite_version",
    "sqlite_version_info",
    "__version__",
]
