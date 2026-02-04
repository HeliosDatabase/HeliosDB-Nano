# HeliosDB SQLite Connection Guide

Comprehensive guide for using HeliosDB's SQLite-compatible connection wrapper with URI support.

## Table of Contents

1. [URI Format Overview](#uri-format-overview)
2. [Connection Examples](#connection-examples)
3. [Mode Switching](#mode-switching)
4. [Connection Pooling](#connection-pooling)
5. [Performance Tuning](#performance-tuning)
6. [Advanced Features](#advanced-features)
7. [Error Handling](#error-handling)

## URI Format Overview

HeliosDB supports multiple URI schemes for maximum compatibility:

### Supported URI Schemes

```
sqlite:///path/to/database.db          # SQLite-compatible file path
sqlite:///:memory:                     # In-memory database
heliosdb://localhost:8080/api          # Remote HeliosDB server
heliosdb:///path/to/database.db        # Local HeliosDB with explicit scheme
file:///absolute/path/to/db.db         # File URI format
```

### URI Components

```
scheme://[host[:port]]/path[?parameters]

scheme:      sqlite | heliosdb | file
host:        localhost | 192.168.1.100 | db.example.com
port:        8080 | 6543 | 5432 (default varies by mode)
path:        /path/to/db.db | :memory: | /api/v1
parameters:  mode=repl&cache=shared&timeout=5000
```

### Common URI Parameters

| Parameter | Values | Description | Example |
|-----------|--------|-------------|---------|
| `mode` | `repl`, `server`, `daemon`, `auto` | HeliosDB operational mode | `?mode=daemon` |
| `mode` | `ro`, `rw`, `rwc`, `memory` | SQLite open mode | `?mode=ro` |
| `cache` | `shared`, `private` | Cache sharing mode | `?cache=shared` |
| `port` | `1-65535` | Daemon port override | `?port=6543` |
| `vfs` | String | Virtual file system | `?vfs=unix-dotfile` |
| `timeout` | Milliseconds | Busy timeout | `?timeout=5000` |

## Connection Examples

### Basic File Database

```python
from heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER import connect

# Simple file database
with connect("sqlite:///myapp.db") as manager:
    result = manager.execute("SELECT * FROM users")
    print(result.to_dicts())
```

### In-Memory Database

```python
# In-memory database for testing
with connect("sqlite:///:memory:") as manager:
    manager.execute("CREATE TABLE test (id INT, name TEXT)")
    manager.execute("INSERT INTO test VALUES (1, 'Alice')")
    result = manager.execute("SELECT * FROM test")
```

### Remote Server Connection

```python
# Connect to remote HeliosDB server
with connect(
    "heliosdb://localhost:8080",
    api_key="your-api-key-here"
) as manager:
    result = manager.execute("SELECT * FROM analytics")
```

### Environment Variable Interpolation

```python
import os

# Set environment variables
os.environ["DB_HOST"] = "localhost"
os.environ["DB_PORT"] = "8080"
os.environ["DB_PATH"] = "/data/production"

# Use in connection string
with connect("heliosdb://${DB_HOST}:${DB_PORT}") as manager:
    result = manager.execute("SELECT version()")

# File path interpolation
with connect("sqlite:///${DB_PATH}/app.db") as manager:
    result = manager.execute("SELECT * FROM config")
```

### Read-Only Database Access

```python
# Read-only mode for safety
with connect("sqlite:///production.db?mode=ro") as manager:
    # This will succeed
    result = manager.execute("SELECT * FROM data")

    # This will fail (read-only mode)
    # manager.execute("DELETE FROM data")
```

## Mode Switching

HeliosDB supports multiple operational modes:

### REPL Mode (Direct Embedded)

Best for single-process applications and local development.

```python
# Explicit REPL mode
with connect("heliosdb:///mydb.db?mode=repl") as manager:
    result = manager.execute("SELECT * FROM local_data")
```

**Characteristics:**
- Direct file access
- No network overhead
- Single process only
- Fast local queries
- No daemon required

### Server Mode (REST API)

Best for multi-process applications and remote access.

```python
# Server mode with REST API
with connect(
    "heliosdb://localhost:8080?mode=server",
    api_key="your-api-key"
) as manager:
    result = manager.execute("SELECT * FROM distributed_data")
```

**Characteristics:**
- Multi-process safe
- Network-based
- Authentication support
- Horizontal scaling
- Remote access

### Daemon Mode

Best for background services and shared access.

```python
# Daemon mode on custom port
with connect("heliosdb:///shared.db?mode=daemon&port=6543") as manager:
    result = manager.execute("SELECT * FROM shared_data")
```

**Characteristics:**
- Background process
- Multi-client support
- Persistent connections
- Shared database access
- Automatic lifecycle management

### Auto Mode (Recommended)

Automatically detects the best mode based on URI.

```python
# Auto-detect mode (default)
with connect("sqlite:///local.db?mode=auto") as manager:
    # Uses REPL mode for local file
    result = manager.execute("SELECT * FROM users")

with connect("heliosdb://localhost:8080?mode=auto") as manager:
    # Uses Server mode for remote host
    result = manager.execute("SELECT * FROM users")
```

**Auto-detection rules:**
- Remote host (hostname:port) → Server mode
- Local file path → REPL mode
- In-memory (`:memory:`) → REPL mode

## Connection Pooling

Connection pooling improves performance for multi-threaded applications.

### Enabling Connection Pooling

```python
# Enable pooling with custom limits
with connect(
    "heliosdb://localhost:8080",
    enable_pooling=True,
    min_connections=2,
    max_connections=10,
    pool_timeout=30.0
) as manager:
    # Pool automatically manages connections
    result = manager.execute("SELECT * FROM data")
```

### Manual Pool Management

```python
from heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER import ConnectionPool

# Create pool
pool = ConnectionPool(
    "sqlite:///mydb.db",
    min_connections=5,
    max_connections=20,
    connection_lifetime=3600.0  # 1 hour
)

# Get connection from pool
with pool.get_connection() as conn:
    result = conn.execute("SELECT * FROM users WHERE active = ?", [True])

# Close all pool connections
pool.close_all()
```

### Multi-threaded Usage

```python
import concurrent.futures
from heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER import ConnectionPool

pool = ConnectionPool(
    "sqlite:///analytics.db",
    max_connections=10
)

def process_batch(batch_id: int):
    with pool.get_connection() as conn:
        result = conn.execute(
            "SELECT * FROM events WHERE batch_id = ?",
            [batch_id]
        )
        return result.to_dicts()

# Process batches in parallel
with concurrent.futures.ThreadPoolExecutor(max_workers=10) as executor:
    futures = [executor.submit(process_batch, i) for i in range(100)]
    results = [f.result() for f in concurrent.futures.as_completed(futures)]

pool.close_all()
```

## Performance Tuning

### Connection Configuration

```python
# Optimize for performance
with connect(
    "sqlite:///large_db.db",
    # Connection timeouts
    connect_timeout=10.0,
    query_timeout=60.0,

    # Pooling for multi-threaded
    enable_pooling=True,
    max_connections=20,
    connection_lifetime=7200.0,  # 2 hours

    # Auto-reconnect
    enable_auto_reconnect=True,
    max_retries=3,
    retry_delay=1.0,
    retry_backoff=2.0,

    # Health checking
    enable_health_check=True,
    health_check_interval=60.0
) as manager:
    result = manager.execute("SELECT * FROM big_table")
```

### SQLite-Specific Optimizations

```python
# Shared cache for better concurrency
uri = "sqlite:///shared.db?cache=shared&timeout=10000"
with connect(uri) as manager:
    result = manager.execute("SELECT * FROM data")

# WAL mode for concurrent reads/writes
with connect("sqlite:///wal.db") as manager:
    manager.execute("PRAGMA journal_mode=WAL")
    manager.execute("PRAGMA synchronous=NORMAL")
```

### Batch Operations

```python
# Efficient batch inserts
with connect("sqlite:///batch.db") as manager:
    # Use transaction for batch
    with manager.get_connection() as conn:
        conn.execute("BEGIN TRANSACTION")

        for i in range(10000):
            conn.execute(
                "INSERT INTO logs VALUES (?, ?, ?)",
                [i, "event", "data"]
            )

        conn.execute("COMMIT")
```

## Advanced Features

### Custom Callbacks

```python
def on_connect(conn):
    """Called when connection is established."""
    print(f"Connected: {conn._connection_id}")
    conn.execute("PRAGMA foreign_keys = ON")

def on_disconnect(conn):
    """Called when connection is closed."""
    print(f"Disconnected: {conn._connection_id}")

def on_error(error):
    """Called when error occurs."""
    print(f"Error: {error}")

# Use callbacks
with connect(
    "sqlite:///mydb.db",
    on_connect=on_connect,
    on_disconnect=on_disconnect,
    on_error=on_error
) as manager:
    result = manager.execute("SELECT * FROM data")
```

### Connection Metrics

```python
from heliosdb.HELIOSDB_SQLITE_CONNECTION_WRAPPER import Connection

with connect("sqlite:///metrics.db", enable_pooling=False) as manager:
    conn = manager.connection

    # Execute queries
    for _ in range(100):
        conn.execute("SELECT * FROM data")

    # Access metrics
    metrics = conn.metrics
    print(f"Total queries: {metrics.total_queries}")
    print(f"Success rate: {metrics.success_rate:.2%}")
    print(f"Avg query time: {metrics.average_query_time_ms:.2f}ms")
    print(f"Connection age: {metrics.age_seconds:.0f}s")
    print(f"Idle time: {metrics.idle_seconds:.0f}s")
```

### URI Parsing

```python
from heliosdb.HELIOSDB_SQLITE_URI_PARSER import parse_uri

# Parse and inspect URI
uri = "heliosdb://localhost:8080/api?mode=server&cache=shared"
parsed = parse_uri(uri)

print(f"Scheme: {parsed.scheme}")
print(f"Host: {parsed.host}")
print(f"Port: {parsed.port}")
print(f"Mode: {parsed.effective_mode}")
print(f"Is remote: {parsed.is_remote}")
print(f"Connection string: {parsed.connection_string}")

# Convert to dict
print(parsed.to_dict())
```

## Error Handling

### Connection Errors

```python
from heliosdb.exceptions import ConnectionError, HeliosDBError

try:
    with connect("sqlite:///missing.db?mode=ro") as manager:
        result = manager.execute("SELECT * FROM data")
except ConnectionError as e:
    print(f"Connection failed: {e}")
    # Handle connection failure
```

### Auto-Reconnect

```python
# Enable automatic reconnection
with connect(
    "heliosdb://unreliable-server:8080",
    enable_auto_reconnect=True,
    max_retries=5,
    retry_delay=2.0,
    retry_backoff=1.5
) as manager:
    try:
        result = manager.execute("SELECT * FROM data")
    except HeliosDBError as e:
        print(f"Query failed after retries: {e}")
```

### Health Checking

```python
with connect(
    "sqlite:///mydb.db",
    enable_health_check=True,
    health_check_interval=30.0,
    health_check_timeout=5.0,
    enable_pooling=False
) as manager:
    conn = manager.connection

    # Check connection health
    if conn.is_healthy:
        result = conn.execute("SELECT * FROM data")
    else:
        print("Connection unhealthy, reconnecting...")
        conn.reconnect()
```

### Graceful Degradation

```python
def query_with_fallback(primary_uri: str, fallback_uri: str, query: str):
    """Try primary connection, fall back to secondary."""
    try:
        with connect(primary_uri) as manager:
            return manager.execute(query)
    except ConnectionError:
        print("Primary connection failed, using fallback...")
        with connect(fallback_uri) as manager:
            return manager.execute(query)

# Usage
result = query_with_fallback(
    "heliosdb://primary:8080",
    "sqlite:///local_cache.db",
    "SELECT * FROM data"
)
```

## Best Practices

### 1. Use Context Managers

Always use `with` statements for automatic cleanup:

```python
# Good
with connect("sqlite:///mydb.db") as manager:
    result = manager.execute("SELECT * FROM data")

# Avoid
manager = connect("sqlite:///mydb.db")
result = manager.execute("SELECT * FROM data")
manager.close()  # Easy to forget!
```

### 2. Enable Pooling for Multi-threaded Apps

```python
# Multi-threaded applications
with connect(
    "sqlite:///shared.db",
    enable_pooling=True,
    max_connections=20
) as manager:
    # Pool handles thread safety
    result = manager.execute("SELECT * FROM data")
```

### 3. Use Environment Variables for Configuration

```python
# Store sensitive data in environment
import os
os.environ["HELIOS_API_KEY"] = "secret-key"
os.environ["HELIOS_DB_URL"] = "heliosdb://prod.example.com"

# Use in code
with connect(
    os.environ["HELIOS_DB_URL"],
    api_key=os.environ["HELIOS_API_KEY"]
) as manager:
    result = manager.execute("SELECT * FROM data")
```

### 4. Monitor Connection Metrics

```python
# Track performance
with connect("sqlite:///analytics.db", enable_pooling=False) as manager:
    conn = manager.connection

    # Your queries
    for i in range(1000):
        conn.execute("SELECT * FROM events WHERE id = ?", [i])

    # Check metrics
    if conn.metrics.average_query_time_ms > 100:
        print("Queries are slow, consider optimization")
```

### 5. Handle Errors Gracefully

```python
from heliosdb.exceptions import ConnectionError, QueryError

try:
    with connect("sqlite:///mydb.db") as manager:
        result = manager.execute("SELECT * FROM data")
except ConnectionError as e:
    print(f"Connection error: {e}")
    # Fall back to cached data
except QueryError as e:
    print(f"Query error: {e}")
    # Log error and return empty result
```

## Troubleshooting

### Connection Refused

```python
# Check if server is running
with connect(
    "heliosdb://localhost:8080",
    connect_timeout=5.0,
    max_retries=3
) as manager:
    result = manager.execute("SELECT 1")
```

### Database Locked

```python
# Use WAL mode and shared cache
uri = "sqlite:///shared.db?cache=shared"
with connect(uri) as manager:
    manager.execute("PRAGMA journal_mode=WAL")
    result = manager.execute("SELECT * FROM data")
```

### Slow Queries

```python
# Enable query logging and metrics
import logging
logging.basicConfig(level=logging.DEBUG)

with connect("sqlite:///slow.db", enable_pooling=False) as manager:
    conn = manager.connection

    result = conn.execute("SELECT * FROM big_table")

    print(f"Query time: {conn.metrics.average_query_time_ms:.2f}ms")
```

## Summary

The HeliosDB SQLite connection wrapper provides:

- Full SQLite URI compatibility
- Automatic mode detection (REPL, Server, Daemon)
- Connection pooling for multi-threaded applications
- Health checking and auto-reconnect
- Comprehensive metrics and monitoring
- Environment variable support
- Production-ready error handling

For more information, see the API documentation and examples in the HeliosDB repository.
