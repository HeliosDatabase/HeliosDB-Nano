"""
HeliosDB Python SDK

Official Python client for HeliosDB Nano - an AI-native embedded database with
PostgreSQL compatibility, vector search, time-travel, and branching.

Example usage:
    from heliosdb import HeliosDB, connect

    # Connect to database (classic API)
    db = HeliosDB("./myapp.db")  # Embedded mode
    # or
    db = HeliosDB.connect("http://localhost:8080")  # REST API mode

    # SQLite-compatible URI connections (NEW)
    with connect("sqlite:///myapp.db") as manager:
        result = manager.execute("SELECT * FROM users WHERE active = ?", [True])

    # Execute queries
    users = db.query("SELECT * FROM users WHERE active = $1", [True])

    # Vector operations
    store = db.vector_store("documents", dimension=1536)
    store.add_texts(["Hello world", "AI is amazing"])
    results = store.similarity_search("greeting", k=5)

    # Agent memory
    memory = db.agent_memory("agent-1")
    memory.save_context({"input": "Hi"}, {"output": "Hello!"})

    # Branching
    with db.branch("experiment-1") as branch:
        branch.execute("UPDATE config SET value = 'test'")
"""

from heliosdb.client import HeliosDB, HeliosDBConfig
from heliosdb.vector import VectorStore
from heliosdb.memory import AgentMemory
from heliosdb.branch import Branch, BranchContext
from heliosdb.models import (
    QueryResult,
    TableInfo,
    TableSchema,
    ColumnDefinition,
    VectorSearchResult,
    MemoryMessage,
    Document,
    ChatSession,
)
from heliosdb.exceptions import (
    HeliosDBError,
    ConnectionError,
    QueryError,
    AuthenticationError,
    NotFoundError,
    ConflictError,
    ValidationError,
)

# SQLite-compatible connection wrapper (NEW)
from heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER import (
    connect,
    ConnectionManager,
    ConnectionPool,
    Connection,
    ConnectionConfig,
    ConnectionMetrics,
    ConnectionState,
)
from heliosdb.HELIOSDB_SQLITE_URI_PARSER import (
    parse_uri,
    URIParser,
    ParsedURI,
    URIScheme,
    HeliosDBMode,
    SQLiteOpenMode,
    CacheMode,
)

__version__ = "3.0.0"
__all__ = [
    # Main client
    "HeliosDB",
    "HeliosDBConfig",
    # Vector operations
    "VectorStore",
    # Agent memory
    "AgentMemory",
    # Branching
    "Branch",
    "BranchContext",
    # Models
    "QueryResult",
    "TableInfo",
    "TableSchema",
    "ColumnDefinition",
    "VectorSearchResult",
    "MemoryMessage",
    "Document",
    "ChatSession",
    # Exceptions
    "HeliosDBError",
    "ConnectionError",
    "QueryError",
    "AuthenticationError",
    "NotFoundError",
    "ConflictError",
    "ValidationError",
    # SQLite-compatible connection wrapper
    "connect",
    "ConnectionManager",
    "ConnectionPool",
    "Connection",
    "ConnectionConfig",
    "ConnectionMetrics",
    "ConnectionState",
    # URI parser
    "parse_uri",
    "URIParser",
    "ParsedURI",
    "URIScheme",
    "HeliosDBMode",
    "SQLiteOpenMode",
    "CacheMode",
]
