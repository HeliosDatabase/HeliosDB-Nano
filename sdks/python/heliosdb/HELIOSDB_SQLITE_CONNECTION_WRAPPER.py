"""
HeliosDB SQLite Connection Wrapper

Production-ready connection manager with SQLite URI support, connection pooling,
mode switching, and lifecycle management.

Features:
- Full SQLite URI support (sqlite://, heliosdb://, file://)
- Connection pooling with configurable limits
- Thread-local connection storage
- Context manager support
- Automatic mode detection (REPL vs Server)
- Environment variable interpolation
- Connection lifecycle management
- Health checking and auto-reconnect
- Comprehensive error handling

Examples:
    # Basic usage with context manager
    with ConnectionManager("sqlite:///mydb.db") as conn:
        result = conn.execute("SELECT * FROM users")

    # Connection pooling
    pool = ConnectionPool("heliosdb://localhost:8080", max_connections=10)
    with pool.get_connection() as conn:
        conn.execute("INSERT INTO logs VALUES (?, ?)", [1, "event"])

    # Environment variable interpolation
    conn = ConnectionManager("sqlite:///${DB_PATH}/app.db")

    # Explicit mode specification
    conn = ConnectionManager("heliosdb:///mydb.db?mode=daemon&port=6543")
"""

from __future__ import annotations

import logging
import os
import threading
import time
from collections import deque
from contextlib import contextmanager
from dataclasses import dataclass, field
from enum import Enum
from threading import Lock, RLock
from typing import Any, Callable, Dict, Generator, List, Optional, Union

from heliosdb.HELIOSDB_SQLITE_URI_PARSER import (
    CacheMode,
    HeliosDBMode,
    ParsedURI,
    SQLiteOpenMode,
    URIParser,
)
from heliosdb.client import HeliosDB, HeliosDBConfig
from heliosdb.exceptions import ConnectionError, HeliosDBError


logger = logging.getLogger(__name__)


class ConnectionState(str, Enum):
    """Connection lifecycle states."""

    DISCONNECTED = "disconnected"
    CONNECTING = "connecting"
    CONNECTED = "connected"
    IDLE = "idle"
    BUSY = "busy"
    ERROR = "error"
    CLOSED = "closed"


@dataclass
class ConnectionMetrics:
    """Connection performance metrics."""

    total_queries: int = 0
    successful_queries: int = 0
    failed_queries: int = 0
    total_time_ms: float = 0.0
    created_at: float = field(default_factory=time.time)
    last_used_at: float = field(default_factory=time.time)
    error_count: int = 0
    reconnect_count: int = 0

    @property
    def average_query_time_ms(self) -> float:
        """Calculate average query execution time."""
        if self.successful_queries == 0:
            return 0.0
        return self.total_time_ms / self.successful_queries

    @property
    def success_rate(self) -> float:
        """Calculate query success rate."""
        if self.total_queries == 0:
            return 1.0
        return self.successful_queries / self.total_queries

    @property
    def age_seconds(self) -> float:
        """Get connection age in seconds."""
        return time.time() - self.created_at

    @property
    def idle_seconds(self) -> float:
        """Get idle time in seconds."""
        return time.time() - self.last_used_at


@dataclass
class ConnectionConfig:
    """Configuration for connection management."""

    # Connection parameters
    uri: str
    expand_env: bool = True
    auto_connect: bool = True

    # Pooling
    enable_pooling: bool = False
    min_connections: int = 1
    max_connections: int = 10
    pool_timeout: float = 30.0
    connection_lifetime: float = 3600.0  # 1 hour

    # Health checking
    enable_health_check: bool = True
    health_check_interval: float = 60.0
    health_check_timeout: float = 5.0

    # Retry configuration
    enable_auto_reconnect: bool = True
    max_retries: int = 3
    retry_delay: float = 1.0
    retry_backoff: float = 2.0

    # Timeouts
    connect_timeout: float = 10.0
    query_timeout: float = 30.0

    # Callbacks
    on_connect: Optional[Callable[[Any], None]] = None
    on_disconnect: Optional[Callable[[Any], None]] = None
    on_error: Optional[Callable[[Exception], None]] = None

    # Additional HeliosDB config
    api_key: Optional[str] = None
    jwt_token: Optional[str] = None
    extra_headers: Dict[str, str] = field(default_factory=dict)


class Connection:
    """
    Managed database connection with lifecycle tracking.

    Wraps either a direct embedded connection or a REST API client,
    providing a unified interface.
    """

    def __init__(
        self,
        parsed_uri: ParsedURI,
        config: ConnectionConfig,
    ):
        """
        Initialize connection.

        Args:
            parsed_uri: Parsed database URI
            config: Connection configuration
        """
        self.parsed_uri = parsed_uri
        self.config = config
        self.state = ConnectionState.DISCONNECTED
        self.metrics = ConnectionMetrics()
        self._client: Optional[HeliosDB] = None
        self._lock = RLock()
        self._connection_id = id(self)

        logger.debug(
            f"Created connection {self._connection_id} for {parsed_uri.connection_string}"
        )

    @property
    def client(self) -> HeliosDB:
        """Get the underlying HeliosDB client."""
        if self._client is None:
            raise ConnectionError("Connection is not established")
        return self._client

    @property
    def is_connected(self) -> bool:
        """Check if connection is established."""
        if self.state != ConnectionState.CONNECTED:
            return False
        # Connected if either REST client or embedded connection is available
        has_rest_client = self._client is not None
        has_embedded_conn = hasattr(self, '_embedded_conn') and self._embedded_conn is not None
        return has_rest_client or has_embedded_conn

    @property
    def is_healthy(self) -> bool:
        """Check connection health."""
        if not self.is_connected:
            return False

        try:
            # Simple health check query
            self.execute("SELECT 1")
            return True
        except Exception:
            return False

    def connect(self) -> None:
        """Establish database connection."""
        with self._lock:
            if self.is_connected:
                logger.debug(f"Connection {self._connection_id} already connected")
                return

            logger.info(
                f"Connecting to {self.parsed_uri.connection_string} "
                f"(mode: {self.parsed_uri.effective_mode.value})"
            )

            self.state = ConnectionState.CONNECTING

            try:
                # Create HeliosDB client based on mode
                if self.parsed_uri.is_remote:
                    # Server mode - REST API client
                    helios_config = HeliosDBConfig(
                        url=self.parsed_uri.connection_string,
                        api_key=self.config.api_key,
                        jwt_token=self.config.jwt_token,
                        connect_timeout=self.config.connect_timeout,
                        read_timeout=self.config.query_timeout,
                        write_timeout=self.config.query_timeout,
                        max_retries=self.config.max_retries,
                        retry_delay=self.config.retry_delay,
                        extra_headers=self.config.extra_headers,
                    )
                    self._client = HeliosDB(config=helios_config)
                    self._embedded_conn = None
                else:
                    # Embedded mode - use SQLite compatibility layer with REPL subprocess
                    try:
                        from heliosdb_sqlite import connect as sqlite_connect
                        # Use 'path' attribute from ParsedURI, default to ':memory:'
                        db_path = self.parsed_uri.path if self.parsed_uri.path else ':memory:'
                        if self.parsed_uri.is_memory:
                            db_path = ':memory:'
                        self._embedded_conn = sqlite_connect(
                            db_path,
                            timeout=self.config.connect_timeout,
                            isolation_level=None,  # Autocommit mode
                            check_same_thread=False,
                        )
                        self._client = None  # REST client not used in embedded mode
                    except ImportError:
                        raise ConnectionError(
                            "Embedded mode requires heliosdb_sqlite package. "
                            "Install it or use server mode (heliosdb:// URL)."
                        )

                self.state = ConnectionState.CONNECTED
                self.metrics.last_used_at = time.time()

                # Execute on_connect callback
                if self.config.on_connect:
                    try:
                        self.config.on_connect(self)
                    except Exception as e:
                        logger.warning(f"on_connect callback failed: {e}")

                logger.info(f"Connection {self._connection_id} established")

            except Exception as e:
                self.state = ConnectionState.ERROR
                self.metrics.error_count += 1

                if self.config.on_error:
                    try:
                        self.config.on_error(e)
                    except Exception as callback_error:
                        logger.warning(f"on_error callback failed: {callback_error}")

                logger.error(f"Connection failed: {e}")
                raise ConnectionError(f"Failed to connect: {e}") from e

    def disconnect(self) -> None:
        """Close database connection."""
        with self._lock:
            if not self.is_connected:
                return

            logger.info(f"Disconnecting connection {self._connection_id}")

            try:
                # Close embedded connection if present
                if hasattr(self, '_embedded_conn') and self._embedded_conn is not None:
                    try:
                        self._embedded_conn.close()
                    except Exception as e:
                        logger.warning(f"Error closing embedded connection: {e}")
                    self._embedded_conn = None

                # Close REST API client if present
                if self._client:
                    if hasattr(self._client, "close"):
                        self._client.close()

                # Execute on_disconnect callback
                if self.config.on_disconnect:
                    try:
                        self.config.on_disconnect(self)
                    except Exception as e:
                        logger.warning(f"on_disconnect callback failed: {e}")

            except Exception as e:
                logger.warning(f"Error during disconnect: {e}")
            finally:
                self._client = None
                if hasattr(self, '_embedded_conn'):
                    self._embedded_conn = None
                self.state = ConnectionState.CLOSED

    def reconnect(self) -> None:
        """Reconnect to database."""
        logger.info(f"Reconnecting connection {self._connection_id}")
        self.disconnect()
        self.connect()
        self.metrics.reconnect_count += 1

    def execute(
        self,
        query: str,
        params: Optional[Union[List[Any], Dict[str, Any]]] = None,
        timeout: Optional[float] = None,
    ) -> Any:
        """
        Execute a query.

        Args:
            query: SQL query string
            params: Query parameters
            timeout: Query timeout (overrides default)

        Returns:
            Query result

        Raises:
            ConnectionError: If connection is not established
            QueryError: If query execution fails
        """
        if not self.is_connected:
            if self.config.enable_auto_reconnect:
                self.connect()
            else:
                raise ConnectionError("Connection is not established")

        start_time = time.time()
        self.state = ConnectionState.BUSY
        self.metrics.total_queries += 1

        try:
            # Execute query based on mode (embedded or server)
            if hasattr(self, '_embedded_conn') and self._embedded_conn is not None:
                # Embedded mode - use SQLite compatibility layer
                cursor = self._embedded_conn.execute(query, params or ())
                if cursor.description:
                    # SELECT query - return dict with rows and columns
                    columns = [desc[0] for desc in cursor.description]
                    rows = cursor.fetchall()
                    result = {'columns': columns, 'rows': rows, 'row_count': len(rows)}
                else:
                    # Non-SELECT - return row count
                    result = {'row_count': cursor.rowcount}
            else:
                # Server mode - use REST API client
                result = self.client.query(query, params)

            # Update metrics
            elapsed_ms = (time.time() - start_time) * 1000
            self.metrics.successful_queries += 1
            self.metrics.total_time_ms += elapsed_ms
            self.metrics.last_used_at = time.time()

            logger.debug(f"Query executed in {elapsed_ms:.2f}ms")

            return result

        except Exception as e:
            self.metrics.failed_queries += 1
            self.metrics.error_count += 1
            self.state = ConnectionState.ERROR

            logger.error(f"Query execution failed: {e}")

            # Auto-reconnect on connection errors
            if self.config.enable_auto_reconnect and isinstance(e, ConnectionError):
                logger.info("Attempting auto-reconnect...")
                try:
                    self.reconnect()
                except Exception as reconnect_error:
                    logger.error(f"Auto-reconnect failed: {reconnect_error}")

            raise

        finally:
            if self.is_connected:
                self.state = ConnectionState.IDLE

    def __enter__(self) -> Connection:
        """Context manager entry."""
        if not self.is_connected:
            self.connect()
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        """Context manager exit."""
        # Don't close in pooled mode - pool will manage lifecycle
        if not self.config.enable_pooling:
            self.disconnect()

    def __del__(self):
        """Cleanup on deletion."""
        try:
            if self.is_connected:
                self.disconnect()
        except Exception:
            pass


class ConnectionPool:
    """
    Thread-safe connection pool with lifecycle management.

    Maintains a pool of reusable connections with automatic
    health checking and connection recycling.
    """

    def __init__(self, uri: str, **config_kwargs):
        """
        Initialize connection pool.

        Args:
            uri: Database URI
            **config_kwargs: Configuration parameters
        """
        self.config = ConnectionConfig(uri=uri, enable_pooling=True, **config_kwargs)
        self.parsed_uri = URIParser.parse(uri, expand_env=self.config.expand_env)

        self._pool: deque[Connection] = deque()
        self._active_connections: Dict[int, Connection] = {}
        self._lock = Lock()
        self._thread_local = threading.local()

        # Initialize minimum connections
        if self.config.auto_connect:
            for _ in range(self.config.min_connections):
                conn = self._create_connection()
                conn.connect()
                self._pool.append(conn)

        logger.info(
            f"Connection pool initialized with {self.config.min_connections} connections"
        )

    def _create_connection(self) -> Connection:
        """Create a new connection."""
        return Connection(self.parsed_uri, self.config)

    @contextmanager
    def get_connection(self, timeout: Optional[float] = None) -> Generator[Connection, None, None]:
        """
        Get a connection from the pool.

        Args:
            timeout: Maximum wait time for available connection

        Yields:
            Connection object

        Raises:
            ConnectionError: If no connection available within timeout
        """
        timeout = timeout or self.config.pool_timeout
        start_time = time.time()
        conn: Optional[Connection] = None

        try:
            # Try to get a connection from the pool
            while conn is None:
                with self._lock:
                    if self._pool:
                        conn = self._pool.popleft()
                        self._active_connections[id(conn)] = conn
                        break

                    # Create new connection if under max
                    if len(self._active_connections) < self.config.max_connections:
                        conn = self._create_connection()
                        conn.connect()
                        self._active_connections[id(conn)] = conn
                        break

                # Check timeout
                if time.time() - start_time > timeout:
                    raise ConnectionError(
                        f"No connection available within {timeout}s timeout"
                    )

                # Wait briefly before retrying
                time.sleep(0.1)

            # Ensure connection is healthy
            if not conn.is_healthy:
                logger.warning("Unhealthy connection detected, reconnecting...")
                conn.reconnect()

            yield conn

        finally:
            # Return connection to pool
            if conn is not None:
                with self._lock:
                    self._active_connections.pop(id(conn), None)

                    # Check if connection should be recycled
                    if (
                        conn.metrics.age_seconds > self.config.connection_lifetime
                        or not conn.is_healthy
                    ):
                        logger.debug(f"Recycling connection {id(conn)}")
                        conn.disconnect()
                    else:
                        self._pool.append(conn)

    def close_all(self) -> None:
        """Close all connections in the pool."""
        with self._lock:
            # Close pooled connections
            while self._pool:
                conn = self._pool.popleft()
                conn.disconnect()

            # Close active connections
            for conn in list(self._active_connections.values()):
                conn.disconnect()

            self._active_connections.clear()

        logger.info("All pool connections closed")

    def __enter__(self) -> ConnectionPool:
        """Context manager entry."""
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        """Context manager exit."""
        self.close_all()


class ConnectionManager:
    """
    High-level connection manager with automatic mode detection.

    Provides a simple interface for database connections with
    automatic pooling, thread-local storage, and lifecycle management.
    """

    def __init__(self, uri: str, **config_kwargs):
        """
        Initialize connection manager.

        Args:
            uri: Database URI
            **config_kwargs: Configuration parameters
        """
        self.uri = uri
        self.config = ConnectionConfig(uri=uri, **config_kwargs)
        self.parsed_uri = URIParser.parse(uri, expand_env=self.config.expand_env)

        # Use pooling for remote connections by default
        if self.parsed_uri.is_remote and "enable_pooling" not in config_kwargs:
            self.config.enable_pooling = True

        if self.config.enable_pooling:
            self._pool = ConnectionPool(uri, **config_kwargs)
            self._connection: Optional[Connection] = None
        else:
            self._connection = Connection(self.parsed_uri, self.config)
            self._pool = None

    @property
    def connection(self) -> Connection:
        """Get the current connection."""
        if self.config.enable_pooling:
            raise RuntimeError(
                "Cannot access connection directly in pooling mode. "
                "Use 'with manager.get_connection() as conn:' instead."
            )

        if self._connection is None:
            raise ConnectionError("Connection not initialized")

        if not self._connection.is_connected and self.config.auto_connect:
            self._connection.connect()

        return self._connection

    @contextmanager
    def get_connection(self) -> Generator[Connection, None, None]:
        """
        Get a connection (from pool or direct).

        Yields:
            Connection object
        """
        if self.config.enable_pooling and self._pool:
            with self._pool.get_connection() as conn:
                yield conn
        else:
            if not self._connection.is_connected:
                self._connection.connect()
            yield self._connection

    def execute(
        self,
        query: str,
        params: Optional[Union[List[Any], Dict[str, Any]]] = None,
    ) -> Any:
        """
        Execute a query.

        Args:
            query: SQL query string
            params: Query parameters

        Returns:
            Query result
        """
        with self.get_connection() as conn:
            return conn.execute(query, params)

    def close(self) -> None:
        """Close all connections."""
        if self._pool:
            self._pool.close_all()
        elif self._connection:
            self._connection.disconnect()

    def __enter__(self) -> ConnectionManager:
        """Context manager entry."""
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        """Context manager exit."""
        if not self.config.enable_pooling:
            self.close()

    def __del__(self):
        """Cleanup on deletion."""
        try:
            self.close()
        except Exception:
            pass


# Convenience function
def connect(uri: str, **config_kwargs) -> ConnectionManager:
    """
    Create a connection manager.

    Args:
        uri: Database URI
        **config_kwargs: Configuration parameters

    Returns:
        ConnectionManager instance

    Examples:
        >>> from heliosdb.connection import connect
        >>> with connect("sqlite:///mydb.db") as manager:
        ...     result = manager.execute("SELECT * FROM users")
    """
    return ConnectionManager(uri, **config_kwargs)
