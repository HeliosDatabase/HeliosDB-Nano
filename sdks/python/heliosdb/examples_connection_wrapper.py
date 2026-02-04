"""
HeliosDB Connection Wrapper - Integration Examples

Production-ready examples demonstrating various use cases:
- Basic connection management
- Connection pooling for web applications
- Multi-threaded batch processing
- Microservices architecture
- Error handling and resilience
"""

import os
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import List, Dict, Any

from HELIOSDB_SQLITE_CONNECTION_WRAPPER import (
    ConnectionManager,
    ConnectionPool,
    connect,
)
from HELIOSDB_SQLITE_URI_PARSER import parse_uri


# Example 1: Basic Single-Process Application
def example_basic_usage():
    """
    Basic usage for single-process applications.
    Uses direct embedded connection for maximum performance.
    """
    print("=== Example 1: Basic Usage ===")

    # Simple file database
    with connect("sqlite:///myapp.db") as manager:
        # Create table
        manager.execute("""
            CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                email TEXT UNIQUE,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)

        # Insert data
        manager.execute(
            "INSERT INTO users (name, email) VALUES (?, ?)",
            ["Alice Smith", "alice@example.com"]
        )

        # Query data
        result = manager.execute("SELECT * FROM users")
        for row in result.to_dicts():
            print(f"User: {row}")

    print("Basic usage complete\n")


# Example 2: In-Memory Database for Testing
def example_in_memory_testing():
    """
    Use in-memory databases for fast unit testing.
    Data is lost when connection closes.
    """
    print("=== Example 2: In-Memory Testing ===")

    with connect("sqlite:///:memory:") as manager:
        # Set up test data
        manager.execute("""
            CREATE TABLE products (
                id INTEGER PRIMARY KEY,
                name TEXT,
                price REAL
            )
        """)

        # Bulk insert test data
        test_products = [
            (1, "Widget A", 19.99),
            (2, "Widget B", 29.99),
            (3, "Widget C", 39.99),
        ]

        for product in test_products:
            manager.execute(
                "INSERT INTO products VALUES (?, ?, ?)",
                list(product)
            )

        # Run test queries
        result = manager.execute("SELECT AVG(price) as avg_price FROM products")
        avg_price = result.to_dicts()[0]["avg_price"]
        print(f"Average price: ${avg_price:.2f}")

        assert avg_price == 29.99, "Price calculation incorrect"
        print("Test passed!")

    print("In-memory testing complete\n")


# Example 3: Connection Pooling for Web Applications
def example_web_application():
    """
    Connection pooling for multi-threaded web applications.
    Handles concurrent requests efficiently.
    """
    print("=== Example 3: Web Application with Pooling ===")

    # Create connection pool (shared across all threads)
    pool = ConnectionPool(
        "sqlite:///web_app.db",
        min_connections=5,
        max_connections=20,
        connection_lifetime=3600.0,  # 1 hour
        enable_health_check=True,
    )

    # Simulate web request handler
    def handle_request(request_id: int) -> Dict[str, Any]:
        """Simulate handling a web request."""
        with pool.get_connection() as conn:
            # Simulate query
            conn.execute("""
                CREATE TABLE IF NOT EXISTS requests (
                    id INTEGER PRIMARY KEY,
                    timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP
                )
            """)

            conn.execute("INSERT INTO requests (id) VALUES (?)", [request_id])

            result = conn.execute(
                "SELECT COUNT(*) as total FROM requests"
            )

            return {
                "request_id": request_id,
                "total_requests": result.to_dicts()[0]["total"],
            }

    # Simulate concurrent requests
    with ThreadPoolExecutor(max_workers=10) as executor:
        futures = [
            executor.submit(handle_request, i)
            for i in range(50)
        ]

        results = [f.result() for f in as_completed(futures)]

    print(f"Processed {len(results)} concurrent requests")

    # Cleanup
    pool.close_all()

    print("Web application example complete\n")


# Example 4: Remote Server Connection
def example_remote_server():
    """
    Connect to remote HeliosDB server via REST API.
    Suitable for distributed systems and microservices.
    """
    print("=== Example 4: Remote Server Connection ===")

    # Connect to remote server
    with connect(
        "heliosdb://localhost:8080",
        api_key=os.environ.get("HELIOSDB_API_KEY", "demo-key"),
        enable_pooling=True,
        max_connections=10,
    ) as manager:
        # Execute remote query
        try:
            result = manager.execute("SELECT version() as version")
            version = result.to_dicts()[0]["version"]
            print(f"Connected to HeliosDB version: {version}")

        except Exception as e:
            print(f"Remote connection failed: {e}")
            print("Make sure HeliosDB server is running on localhost:8080")

    print("Remote server example complete\n")


# Example 5: Environment-Based Configuration
def example_environment_config():
    """
    Use environment variables for flexible configuration.
    Best practice for production deployments.
    """
    print("=== Example 5: Environment-Based Configuration ===")

    # Set environment variables
    os.environ["DB_PATH"] = "/tmp/helios"
    os.environ["DB_NAME"] = "production.db"
    os.environ["API_KEY"] = "secret-key"

    # Use in connection string
    with connect("sqlite:///${DB_PATH}/${DB_NAME}") as manager:
        parsed = manager.parsed_uri
        print(f"Database path: {parsed.path}")
        print(f"Connection string: {parsed.connection_string}")

    print("Environment configuration complete\n")


# Example 6: Multi-Database Application
def example_multi_database():
    """
    Manage multiple databases in a single application.
    Common in data warehouse and ETL scenarios.
    """
    print("=== Example 6: Multi-Database Application ===")

    # Primary application database
    app_db = ConnectionManager("sqlite:///app.db")

    # Analytics database
    analytics_db = ConnectionManager("sqlite:///analytics.db")

    # Audit log database (read-only)
    audit_db = ConnectionManager("sqlite:///audit.db?mode=ro")

    try:
        # Write to app database
        with app_db.get_connection() as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS orders (
                    id INTEGER PRIMARY KEY,
                    amount REAL
                )
            """)
            conn.execute("INSERT INTO orders (amount) VALUES (?)", [99.99])

        # Aggregate to analytics
        with analytics_db.get_connection() as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS daily_revenue (
                    date TEXT PRIMARY KEY,
                    total REAL
                )
            """)

        # Read audit logs (read-only)
        with audit_db.get_connection() as conn:
            try:
                result = conn.execute("SELECT COUNT(*) FROM audit_log")
                print(f"Audit entries: {result.to_dicts()}")
            except:
                print("Audit log table doesn't exist yet")

        print("Multi-database operations complete")

    finally:
        app_db.close()
        analytics_db.close()
        audit_db.close()

    print("Multi-database example complete\n")


# Example 7: Batch Processing with Metrics
def example_batch_processing():
    """
    High-performance batch processing with connection pooling.
    Tracks performance metrics.
    """
    print("=== Example 7: Batch Processing with Metrics ===")

    pool = ConnectionPool(
        "sqlite:///batch.db",
        max_connections=10,
        auto_connect=True,
    )

    def process_batch(batch_id: int, records: List[Dict]) -> Dict[str, Any]:
        """Process a batch of records."""
        start_time = time.time()

        with pool.get_connection() as conn:
            # Create table if needed
            conn.execute("""
                CREATE TABLE IF NOT EXISTS processed_data (
                    batch_id INTEGER,
                    record_id INTEGER,
                    value REAL
                )
            """)

            # Process records
            for record in records:
                conn.execute(
                    "INSERT INTO processed_data VALUES (?, ?, ?)",
                    [batch_id, record["id"], record["value"]]
                )

            elapsed = time.time() - start_time

            return {
                "batch_id": batch_id,
                "records": len(records),
                "elapsed_ms": elapsed * 1000,
                "metrics": {
                    "total_queries": conn.metrics.total_queries,
                    "avg_time_ms": conn.metrics.average_query_time_ms,
                }
            }

    # Generate test batches
    batches = [
        {"batch_id": i, "records": [{"id": j, "value": j * 1.5} for j in range(100)]}
        for i in range(10)
    ]

    # Process in parallel
    with ThreadPoolExecutor(max_workers=5) as executor:
        futures = [
            executor.submit(process_batch, batch["batch_id"], batch["records"])
            for batch in batches
        ]

        results = [f.result() for f in as_completed(futures)]

    # Print statistics
    total_records = sum(r["records"] for r in results)
    total_time = sum(r["elapsed_ms"] for r in results)
    avg_time = total_time / len(results)

    print(f"Processed {total_records} records in {len(results)} batches")
    print(f"Average batch time: {avg_time:.2f}ms")

    pool.close_all()

    print("Batch processing complete\n")


# Example 8: Error Handling and Resilience
def example_error_handling():
    """
    Robust error handling with retry logic.
    Demonstrates graceful degradation.
    """
    print("=== Example 8: Error Handling and Resilience ===")

    # Custom error handler
    def handle_error(error):
        print(f"Connection error occurred: {error}")
        # Could send to monitoring system, log, etc.

    # Configure with retry logic
    with connect(
        "sqlite:///resilient.db",
        enable_auto_reconnect=True,
        max_retries=3,
        retry_delay=1.0,
        retry_backoff=2.0,
        on_error=handle_error,
    ) as manager:
        try:
            # This query should work
            manager.execute("SELECT 1")
            print("Query executed successfully")

        except Exception as e:
            print(f"Query failed: {e}")

        # Graceful fallback example
        try:
            result = manager.execute("SELECT * FROM nonexistent_table")
        except Exception as e:
            print(f"Expected error: {e}")
            # Fall back to default data
            result = {"data": []}

    print("Error handling example complete\n")


# Example 9: URI Parsing and Inspection
def example_uri_parsing():
    """
    Parse and inspect database URIs.
    Useful for configuration validation.
    """
    print("=== Example 9: URI Parsing and Inspection ===")

    uris = [
        "sqlite:///mydb.db",
        "sqlite:///:memory:",
        "heliosdb://localhost:8080",
        "sqlite:///shared.db?mode=ro&cache=shared",
        "heliosdb:///daemon.db?mode=daemon&port=6543",
    ]

    for uri in uris:
        parsed = parse_uri(uri)
        print(f"\nURI: {uri}")
        print(f"  Scheme: {parsed.scheme.value}")
        print(f"  Mode: {parsed.effective_mode.value}")
        print(f"  Is remote: {parsed.is_remote}")
        print(f"  Is memory: {parsed.is_memory}")
        print(f"  Connection string: {parsed.connection_string}")

    print("\nURI parsing complete\n")


# Example 10: Custom Connection Lifecycle
def example_lifecycle_hooks():
    """
    Use lifecycle hooks for custom behavior.
    Useful for logging, monitoring, and setup.
    """
    print("=== Example 10: Custom Connection Lifecycle ===")

    def setup_connection(conn):
        """Called when connection is established."""
        print(f"Connection {conn._connection_id} established")
        # Execute initialization queries
        conn.execute("PRAGMA foreign_keys = ON")
        conn.execute("PRAGMA journal_mode = WAL")

    def teardown_connection(conn):
        """Called when connection is closed."""
        print(f"Connection {conn._connection_id} metrics:")
        print(f"  Total queries: {conn.metrics.total_queries}")
        print(f"  Success rate: {conn.metrics.success_rate:.1%}")
        print(f"  Avg query time: {conn.metrics.average_query_time_ms:.2f}ms")

    with connect(
        "sqlite:///lifecycle.db",
        on_connect=setup_connection,
        on_disconnect=teardown_connection,
        enable_pooling=False,  # Disable pooling to see lifecycle
    ) as manager:
        # Execute some queries
        for i in range(10):
            manager.execute("SELECT ?", [i])

    print("Lifecycle hooks example complete\n")


def main():
    """Run all examples."""
    examples = [
        example_basic_usage,
        example_in_memory_testing,
        example_web_application,
        # example_remote_server,  # Uncomment if server is running
        example_environment_config,
        example_multi_database,
        example_batch_processing,
        example_error_handling,
        example_uri_parsing,
        example_lifecycle_hooks,
    ]

    print("=" * 60)
    print("HeliosDB Connection Wrapper - Integration Examples")
    print("=" * 60 + "\n")

    for example in examples:
        try:
            example()
        except Exception as e:
            print(f"Example failed: {e}\n")

    print("=" * 60)
    print("All examples complete!")
    print("=" * 60)


if __name__ == "__main__":
    main()
