"""
Comprehensive unit tests for HeliosDB SQLite Connection Wrapper.

Tests cover:
- URI parsing and validation
- Connection lifecycle management
- Connection pooling
- Error handling and recovery
- Thread safety
- Performance metrics
"""

import os
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import MagicMock, Mock, patch

from HELIOSDB_SQLITE_CONNECTION_WRAPPER import (
    Connection,
    ConnectionConfig,
    ConnectionManager,
    ConnectionMetrics,
    ConnectionPool,
    ConnectionState,
    connect,
)
from HELIOSDB_SQLITE_URI_PARSER import (
    CacheMode,
    HeliosDBMode,
    ParsedURI,
    SQLiteOpenMode,
    URIParser,
    URIScheme,
    parse_uri,
)


class TestURIParser(unittest.TestCase):
    """Test URI parsing functionality."""

    def test_parse_simple_sqlite_uri(self):
        """Test parsing simple SQLite file URI."""
        result = parse_uri("sqlite:///mydb.db")

        self.assertEqual(result.scheme, URIScheme.SQLITE)
        self.assertTrue(result.path.endswith("mydb.db"))
        self.assertFalse(result.is_memory)
        self.assertFalse(result.is_remote)
        self.assertTrue(result.is_embedded)

    def test_parse_memory_uri(self):
        """Test parsing in-memory database URI."""
        result = parse_uri("sqlite:///:memory:")

        self.assertEqual(result.scheme, URIScheme.SQLITE)
        self.assertEqual(result.path, ":memory:")
        self.assertTrue(result.is_memory)
        self.assertFalse(result.is_remote)

    def test_parse_remote_uri(self):
        """Test parsing remote server URI."""
        result = parse_uri("heliosdb://localhost:8080")

        self.assertEqual(result.scheme, URIScheme.HELIOSDB)
        self.assertEqual(result.host, "localhost")
        self.assertEqual(result.port, 8080)
        self.assertTrue(result.is_remote)
        self.assertFalse(result.is_memory)

    def test_parse_uri_with_parameters(self):
        """Test parsing URI with query parameters."""
        result = parse_uri("sqlite:///db.db?mode=ro&cache=shared&timeout=5000")

        self.assertEqual(result.sqlite_mode, SQLiteOpenMode.READ_ONLY)
        self.assertEqual(result.cache_mode, CacheMode.SHARED)
        self.assertEqual(result.parameters["timeout"], "5000")

    def test_parse_uri_with_mode_parameter(self):
        """Test parsing URI with mode parameter."""
        result = parse_uri("heliosdb:///db.db?mode=daemon&port=6543")

        self.assertEqual(result.mode, HeliosDBMode.DAEMON)
        self.assertEqual(result.port, 6543)

    def test_environment_variable_expansion(self):
        """Test environment variable expansion in URIs."""
        os.environ["TEST_DB_PATH"] = "/tmp/test"

        result = parse_uri("sqlite:///${TEST_DB_PATH}/mydb.db", expand_env=True)

        self.assertIn("/tmp/test", result.path)

    def test_invalid_uri_scheme(self):
        """Test error handling for invalid URI scheme."""
        with self.assertRaises(ValueError) as ctx:
            parse_uri("invalid:///mydb.db")

        self.assertIn("Unsupported URI scheme", str(ctx.exception))

    def test_empty_uri(self):
        """Test error handling for empty URI."""
        with self.assertRaises(ValueError) as ctx:
            parse_uri("")

        self.assertIn("cannot be empty", str(ctx.exception))

    def test_effective_mode_auto_detection(self):
        """Test automatic mode detection."""
        # Remote should be Server mode
        remote = parse_uri("heliosdb://localhost:8080")
        self.assertEqual(remote.effective_mode, HeliosDBMode.SERVER)

        # Local file should be REPL mode
        local = parse_uri("sqlite:///db.db")
        self.assertEqual(local.effective_mode, HeliosDBMode.REPL)

        # Memory should be REPL mode
        memory = parse_uri("sqlite:///:memory:")
        self.assertEqual(memory.effective_mode, HeliosDBMode.REPL)

    def test_connection_string_generation(self):
        """Test connection string generation."""
        # Memory database
        memory = parse_uri("sqlite:///:memory:")
        self.assertEqual(memory.connection_string, ":memory:")

        # Remote server
        remote = parse_uri("heliosdb://localhost:8080")
        self.assertEqual(remote.connection_string, "http://localhost:8080")

        # File path
        local = parse_uri("sqlite:///mydb.db")
        self.assertTrue(local.connection_string.endswith("mydb.db"))

    def test_to_dict_serialization(self):
        """Test URI serialization to dictionary."""
        result = parse_uri("heliosdb://localhost:8080?mode=server")
        data = result.to_dict()

        self.assertEqual(data["scheme"], "heliosdb")
        self.assertEqual(data["host"], "localhost")
        self.assertEqual(data["port"], 8080)
        self.assertEqual(data["mode"], "server")
        self.assertEqual(data["is_remote"], True)


class TestConnectionMetrics(unittest.TestCase):
    """Test connection metrics tracking."""

    def test_initial_metrics(self):
        """Test initial metrics values."""
        metrics = ConnectionMetrics()

        self.assertEqual(metrics.total_queries, 0)
        self.assertEqual(metrics.successful_queries, 0)
        self.assertEqual(metrics.failed_queries, 0)
        self.assertEqual(metrics.total_time_ms, 0.0)
        self.assertEqual(metrics.error_count, 0)

    def test_average_query_time(self):
        """Test average query time calculation."""
        metrics = ConnectionMetrics()
        metrics.successful_queries = 10
        metrics.total_time_ms = 1000.0

        self.assertEqual(metrics.average_query_time_ms, 100.0)

    def test_success_rate(self):
        """Test success rate calculation."""
        metrics = ConnectionMetrics()
        metrics.total_queries = 100
        metrics.successful_queries = 95

        self.assertEqual(metrics.success_rate, 0.95)

    def test_age_calculation(self):
        """Test connection age calculation."""
        metrics = ConnectionMetrics()
        time.sleep(0.1)

        self.assertGreater(metrics.age_seconds, 0.0)

    def test_idle_time_calculation(self):
        """Test idle time calculation."""
        metrics = ConnectionMetrics()
        time.sleep(0.1)

        self.assertGreater(metrics.idle_seconds, 0.0)


class TestConnectionConfig(unittest.TestCase):
    """Test connection configuration."""

    def test_default_config(self):
        """Test default configuration values."""
        config = ConnectionConfig(uri="sqlite:///test.db")

        self.assertEqual(config.uri, "sqlite:///test.db")
        self.assertTrue(config.expand_env)
        self.assertTrue(config.auto_connect)
        self.assertFalse(config.enable_pooling)
        self.assertEqual(config.max_connections, 10)

    def test_custom_config(self):
        """Test custom configuration."""
        config = ConnectionConfig(
            uri="heliosdb://localhost:8080",
            enable_pooling=True,
            max_connections=20,
            connect_timeout=15.0,
        )

        self.assertTrue(config.enable_pooling)
        self.assertEqual(config.max_connections, 20)
        self.assertEqual(config.connect_timeout, 15.0)

    def test_config_with_callbacks(self):
        """Test configuration with callbacks."""
        on_connect_called = []
        on_disconnect_called = []

        def on_connect(conn):
            on_connect_called.append(True)

        def on_disconnect(conn):
            on_disconnect_called.append(True)

        config = ConnectionConfig(
            uri="sqlite:///:memory:",
            on_connect=on_connect,
            on_disconnect=on_disconnect,
        )

        self.assertIsNotNone(config.on_connect)
        self.assertIsNotNone(config.on_disconnect)


class TestConnection(unittest.TestCase):
    """Test connection lifecycle management."""

    def setUp(self):
        """Set up test fixtures."""
        self.temp_db = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
        self.temp_db.close()
        self.db_path = self.temp_db.name

    def tearDown(self):
        """Clean up test fixtures."""
        try:
            os.unlink(self.db_path)
        except:
            pass

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_connection_creation(self, mock_heliosdb):
        """Test connection creation."""
        parsed_uri = parse_uri(f"sqlite:///{self.db_path}")
        config = ConnectionConfig(uri=f"sqlite:///{self.db_path}")

        conn = Connection(parsed_uri, config)

        self.assertEqual(conn.state, ConnectionState.DISCONNECTED)
        self.assertIsNone(conn._client)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_connection_connect(self, mock_heliosdb):
        """Test establishing connection."""
        mock_client = MagicMock()
        mock_heliosdb.return_value = mock_client

        parsed_uri = parse_uri("sqlite:///:memory:")
        config = ConnectionConfig(uri="sqlite:///:memory:")

        conn = Connection(parsed_uri, config)
        conn.connect()

        self.assertEqual(conn.state, ConnectionState.CONNECTED)
        self.assertIsNotNone(conn._client)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_connection_disconnect(self, mock_heliosdb):
        """Test closing connection."""
        mock_client = MagicMock()
        mock_client.close = MagicMock()
        mock_heliosdb.return_value = mock_client

        parsed_uri = parse_uri("sqlite:///:memory:")
        config = ConnectionConfig(uri="sqlite:///:memory:")

        conn = Connection(parsed_uri, config)
        conn.connect()
        conn.disconnect()

        self.assertEqual(conn.state, ConnectionState.CLOSED)
        self.assertIsNone(conn._client)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_connection_execute(self, mock_heliosdb):
        """Test query execution."""
        mock_client = MagicMock()
        mock_result = MagicMock()
        mock_client.query.return_value = mock_result
        mock_heliosdb.return_value = mock_client

        parsed_uri = parse_uri("sqlite:///:memory:")
        config = ConnectionConfig(uri="sqlite:///:memory:")

        conn = Connection(parsed_uri, config)
        conn.connect()

        result = conn.execute("SELECT 1")

        self.assertIsNotNone(result)
        mock_client.query.assert_called_once()
        self.assertEqual(conn.metrics.successful_queries, 1)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_connection_context_manager(self, mock_heliosdb):
        """Test connection as context manager."""
        mock_client = MagicMock()
        mock_heliosdb.return_value = mock_client

        parsed_uri = parse_uri("sqlite:///:memory:")
        config = ConnectionConfig(uri="sqlite:///:memory:")

        conn = Connection(parsed_uri, config)

        with conn:
            self.assertTrue(conn.is_connected)

        # Connection should close after context (non-pooled)
        self.assertEqual(conn.state, ConnectionState.CLOSED)


class TestConnectionPool(unittest.TestCase):
    """Test connection pooling functionality."""

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_pool_creation(self, mock_heliosdb):
        """Test connection pool creation."""
        mock_heliosdb.return_value = MagicMock()

        pool = ConnectionPool(
            "sqlite:///:memory:",
            min_connections=2,
            max_connections=5,
            auto_connect=False,
        )

        self.assertIsNotNone(pool)
        self.assertEqual(pool.config.max_connections, 5)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_pool_get_connection(self, mock_heliosdb):
        """Test getting connection from pool."""
        mock_heliosdb.return_value = MagicMock()

        pool = ConnectionPool(
            "sqlite:///:memory:",
            min_connections=1,
            auto_connect=True,
        )

        with pool.get_connection() as conn:
            self.assertIsNotNone(conn)
            self.assertTrue(conn.is_connected)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_pool_connection_reuse(self, mock_heliosdb):
        """Test connection reuse in pool."""
        mock_heliosdb.return_value = MagicMock()

        pool = ConnectionPool(
            "sqlite:///:memory:",
            min_connections=1,
            auto_connect=True,
        )

        conn_id_1 = None
        conn_id_2 = None

        with pool.get_connection() as conn:
            conn_id_1 = id(conn)

        with pool.get_connection() as conn:
            conn_id_2 = id(conn)

        # Should reuse the same connection
        self.assertEqual(conn_id_1, conn_id_2)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_pool_close_all(self, mock_heliosdb):
        """Test closing all pool connections."""
        mock_heliosdb.return_value = MagicMock()

        pool = ConnectionPool(
            "sqlite:///:memory:",
            min_connections=2,
            auto_connect=True,
        )

        pool.close_all()

        self.assertEqual(len(pool._pool), 0)
        self.assertEqual(len(pool._active_connections), 0)


class TestConnectionManager(unittest.TestCase):
    """Test high-level connection manager."""

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_manager_creation(self, mock_heliosdb):
        """Test connection manager creation."""
        mock_heliosdb.return_value = MagicMock()

        manager = ConnectionManager("sqlite:///:memory:")

        self.assertIsNotNone(manager)
        self.assertIsNotNone(manager.parsed_uri)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_manager_execute(self, mock_heliosdb):
        """Test query execution through manager."""
        mock_client = MagicMock()
        mock_result = MagicMock()
        mock_client.query.return_value = mock_result
        mock_heliosdb.return_value = mock_client

        manager = ConnectionManager("sqlite:///:memory:")

        result = manager.execute("SELECT 1")

        self.assertIsNotNone(result)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_manager_context_manager(self, mock_heliosdb):
        """Test manager as context manager."""
        mock_heliosdb.return_value = MagicMock()

        with ConnectionManager("sqlite:///:memory:") as manager:
            self.assertIsNotNone(manager)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_manager_with_pooling(self, mock_heliosdb):
        """Test manager with connection pooling."""
        mock_heliosdb.return_value = MagicMock()

        manager = ConnectionManager(
            "sqlite:///:memory:",
            enable_pooling=True,
            max_connections=5,
        )

        self.assertTrue(manager.config.enable_pooling)
        self.assertIsNotNone(manager._pool)

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_convenience_connect_function(self, mock_heliosdb):
        """Test convenience connect() function."""
        mock_heliosdb.return_value = MagicMock()

        with connect("sqlite:///:memory:") as manager:
            self.assertIsInstance(manager, ConnectionManager)


class TestThreadSafety(unittest.TestCase):
    """Test thread safety of connection pooling."""

    @patch("heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER.HeliosDB")
    def test_concurrent_access(self, mock_heliosdb):
        """Test concurrent access to connection pool."""
        mock_client = MagicMock()
        mock_result = MagicMock()
        mock_client.query.return_value = mock_result
        mock_heliosdb.return_value = mock_client

        pool = ConnectionPool(
            "sqlite:///:memory:",
            max_connections=5,
            auto_connect=True,
        )

        results = []
        errors = []

        def worker(worker_id):
            try:
                with pool.get_connection() as conn:
                    result = conn.execute("SELECT 1")
                    results.append(worker_id)
            except Exception as e:
                errors.append(e)

        # Create 10 threads (more than pool size)
        threads = [threading.Thread(target=worker, args=(i,)) for i in range(10)]

        for t in threads:
            t.start()

        for t in threads:
            t.join()

        # All threads should complete successfully
        self.assertEqual(len(results), 10)
        self.assertEqual(len(errors), 0)

        pool.close_all()


if __name__ == "__main__":
    unittest.main()
