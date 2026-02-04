# HeliosDB SQLite Connection Wrapper

Production-ready connection manager with full SQLite URI support for HeliosDB-Lite.

## Features

- Full SQLite URI compatibility (`sqlite://`, `heliosdb://`, `file://`)
- Automatic mode detection (REPL, Server, Daemon)
- Connection pooling with configurable limits
- Thread-safe connection management
- Health checking and auto-reconnect
- Comprehensive performance metrics
- Environment variable interpolation
- RFC 3986 compliant URI parsing
- Context manager support
- Production-ready error handling

## Quick Start

### Basic Usage

```python
from heliosdb import connect

# Simple file database
with connect("sqlite:///myapp.db") as manager:
    result = manager.execute("SELECT * FROM users")
    print(result.to_dicts())
```

### In-Memory Database

```python
# Perfect for testing
with connect("sqlite:///:memory:") as manager:
    manager.execute("CREATE TABLE test (id INT, name TEXT)")
    manager.execute("INSERT INTO test VALUES (1, 'Alice')")
    result = manager.execute("SELECT * FROM test")
```

### Remote Server

```python
# Connect to HeliosDB server
with connect(
    "heliosdb://localhost:8080",
    api_key="your-api-key"
) as manager:
    result = manager.execute("SELECT version()")
```

## Installation

The connection wrapper is included in the HeliosDB Python SDK:

```bash
pip install heliosdb
```

## URI Format

### Supported Schemes

| Scheme | Description | Example |
|--------|-------------|---------|
| `sqlite://` | SQLite-compatible file or memory database | `sqlite:///mydb.db` |
| `heliosdb://` | Explicit HeliosDB mode | `heliosdb://localhost:8080` |
| `file://` | File URI format | `file:///absolute/path/db.db` |

### URI Structure

```
scheme://[host[:port]]/path[?parameters]
```

**Components:**
- `scheme`: `sqlite`, `heliosdb`, or `file`
- `host`: Server hostname or IP address (for remote connections)
- `port`: Server port (default: 8080 for heliosdb, 5432 for postgres)
- `path`: Database file path or `:memory:`
- `parameters`: Query parameters for configuration

### URI Parameters

| Parameter | Values | Description |
|-----------|--------|-------------|
| `mode` | `repl`, `server`, `daemon`, `auto` | Operational mode |
| `mode` | `ro`, `rw`, `rwc` | SQLite open mode |
| `cache` | `shared`, `private` | Cache sharing mode |
| `port` | `1-65535` | Daemon port override |
| `vfs` | String | Virtual file system |
| `timeout` | Milliseconds | Busy timeout |

### URI Examples

```python
# Local file database
"sqlite:///path/to/database.db"

# In-memory database
"sqlite:///:memory:"

# Remote server
"heliosdb://localhost:8080"

# Read-only database
"sqlite:///readonly.db?mode=ro"

# Shared cache
"sqlite:///shared.db?cache=shared"

# Daemon mode with custom port
"heliosdb:///mydb.db?mode=daemon&port=6543"

# Environment variables
"sqlite:///${DB_PATH}/app.db"
```

## Connection Modes

HeliosDB supports three operational modes:

### REPL Mode (Embedded)

Direct file access for single-process applications.

```python
with connect("sqlite:///mydb.db?mode=repl") as manager:
    result = manager.execute("SELECT * FROM data")
```

**Characteristics:**
- Fastest performance (no network overhead)
- Single process only
- Direct file access
- Best for: CLI tools, embedded apps, desktop software

### Server Mode (REST API)

Network-based access for distributed systems.

```python
with connect("heliosdb://localhost:8080?mode=server") as manager:
    result = manager.execute("SELECT * FROM data")
```

**Characteristics:**
- Multi-process safe
- Remote access
- Authentication support
- Best for: Web apps, microservices, distributed systems

### Daemon Mode

Background process with shared access.

```python
with connect("heliosdb:///shared.db?mode=daemon&port=6543") as manager:
    result = manager.execute("SELECT * FROM data")
```

**Characteristics:**
- Multi-client support
- Persistent connections
- Automatic lifecycle management
- Best for: Shared databases, background services

### Auto Mode (Default)

Automatically detects the best mode based on URI.

```python
# Auto-detects REPL mode for local file
with connect("sqlite:///local.db") as manager:
    result = manager.execute("SELECT * FROM data")

# Auto-detects Server mode for remote host
with connect("heliosdb://localhost:8080") as manager:
    result = manager.execute("SELECT * FROM data")
```

## Connection Pooling

Connection pooling improves performance for multi-threaded applications.

### Enabling Pooling

```python
from heliosdb import connect

# Automatic pooling for remote connections
with connect(
    "heliosdb://localhost:8080",
    enable_pooling=True,
    max_connections=10
) as manager:
    result = manager.execute("SELECT * FROM data")
```

### Manual Pool Management

```python
from heliosdb import ConnectionPool

# Create connection pool
pool = ConnectionPool(
    "sqlite:///mydb.db",
    min_connections=5,
    max_connections=20,
    connection_lifetime=3600.0  # 1 hour
)

# Get connection from pool
with pool.get_connection() as conn:
    result = conn.execute("SELECT * FROM users")

# Close all connections
pool.close_all()
```

### Multi-threaded Usage

```python
from concurrent.futures import ThreadPoolExecutor
from heliosdb import ConnectionPool

pool = ConnectionPool("sqlite:///analytics.db", max_connections=10)

def process_batch(batch_id):
    with pool.get_connection() as conn:
        return conn.execute("SELECT * FROM batch WHERE id = ?", [batch_id])

# Process in parallel
with ThreadPoolExecutor(max_workers=10) as executor:
    results = list(executor.map(process_batch, range(100)))

pool.close_all()
```

## Configuration

### Connection Configuration

```python
from heliosdb import connect

with connect(
    "sqlite:///mydb.db",
    # Timeouts
    connect_timeout=10.0,
    query_timeout=60.0,

    # Pooling
    enable_pooling=True,
    max_connections=20,
    connection_lifetime=3600.0,

    # Auto-reconnect
    enable_auto_reconnect=True,
    max_retries=3,
    retry_delay=1.0,

    # Health checking
    enable_health_check=True,
    health_check_interval=60.0,

    # Authentication (for remote servers)
    api_key="your-api-key",
    jwt_token="your-jwt-token",
) as manager:
    result = manager.execute("SELECT * FROM data")
```

### Environment Variables

```python
import os

# Set environment variables
os.environ["DB_HOST"] = "localhost"
os.environ["DB_PORT"] = "8080"
os.environ["DB_PATH"] = "/data/production"
os.environ["HELIOSDB_API_KEY"] = "secret-key"

# Use in connection
with connect(
    "heliosdb://${DB_HOST}:${DB_PORT}",
    api_key=os.environ["HELIOSDB_API_KEY"]
) as manager:
    result = manager.execute("SELECT * FROM data")
```

## Advanced Features

### Custom Lifecycle Hooks

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
from heliosdb import connect

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
```

### URI Parsing

```python
from heliosdb import parse_uri

# Parse and inspect URI
uri = "heliosdb://localhost:8080/api?mode=server&cache=shared"
parsed = parse_uri(uri)

print(f"Scheme: {parsed.scheme}")
print(f"Host: {parsed.host}")
print(f"Port: {parsed.port}")
print(f"Mode: {parsed.effective_mode}")
print(f"Is remote: {parsed.is_remote}")
print(f"Connection string: {parsed.connection_string}")

# Convert to dictionary
config = parsed.to_dict()
```

## Error Handling

### Connection Errors

```python
from heliosdb import connect
from heliosdb.exceptions import ConnectionError, HeliosDBError

try:
    with connect("sqlite:///missing.db?mode=ro") as manager:
        result = manager.execute("SELECT * FROM data")
except ConnectionError as e:
    print(f"Connection failed: {e}")
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

### Graceful Degradation

```python
def query_with_fallback(primary_uri, fallback_uri, query):
    """Try primary connection, fall back to secondary."""
    try:
        with connect(primary_uri) as manager:
            return manager.execute(query)
    except ConnectionError:
        with connect(fallback_uri) as manager:
            return manager.execute(query)

# Usage
result = query_with_fallback(
    "heliosdb://primary:8080",
    "sqlite:///local_cache.db",
    "SELECT * FROM data"
)
```

## Performance Tuning

### Connection Pooling

For multi-threaded applications, enable connection pooling:

```python
with connect(
    "sqlite:///shared.db",
    enable_pooling=True,
    min_connections=5,
    max_connections=20,
    pool_timeout=30.0
) as manager:
    result = manager.execute("SELECT * FROM data")
```

### SQLite Optimizations

```python
# Shared cache for better concurrency
with connect("sqlite:///shared.db?cache=shared") as manager:
    result = manager.execute("SELECT * FROM data")

# WAL mode for concurrent reads/writes
with connect("sqlite:///wal.db") as manager:
    manager.execute("PRAGMA journal_mode=WAL")
    manager.execute("PRAGMA synchronous=NORMAL")
```

### Batch Operations

```python
# Use transactions for batch inserts
with connect("sqlite:///batch.db") as manager:
    with manager.get_connection() as conn:
        conn.execute("BEGIN TRANSACTION")

        for i in range(10000):
            conn.execute("INSERT INTO logs VALUES (?, ?)", [i, "data"])

        conn.execute("COMMIT")
```

## Best Practices

1. **Always use context managers** for automatic cleanup:
   ```python
   # Good
   with connect("sqlite:///db.db") as manager:
       result = manager.execute("SELECT * FROM data")

   # Avoid
   manager = connect("sqlite:///db.db")
   result = manager.execute("SELECT * FROM data")
   manager.close()  # Easy to forget!
   ```

2. **Enable pooling for multi-threaded apps**:
   ```python
   with connect("sqlite:///db.db", enable_pooling=True) as manager:
       result = manager.execute("SELECT * FROM data")
   ```

3. **Use environment variables for configuration**:
   ```python
   import os
   with connect(
       os.environ["DATABASE_URL"],
       api_key=os.environ.get("HELIOSDB_API_KEY")
   ) as manager:
       result = manager.execute("SELECT * FROM data")
   ```

4. **Monitor connection metrics** in production:
   ```python
   with connect("sqlite:///db.db", enable_pooling=False) as manager:
       conn = manager.connection
       # Execute queries...
       if conn.metrics.average_query_time_ms > 100:
           print("Queries are slow, consider optimization")
   ```

5. **Handle errors gracefully**:
   ```python
   try:
       with connect("sqlite:///db.db") as manager:
           result = manager.execute("SELECT * FROM data")
   except ConnectionError:
       # Fall back to cached data
       result = get_cached_data()
   ```

## API Reference

### Main Functions

- `connect(uri, **config)` - Create connection manager
- `parse_uri(uri, expand_env=True)` - Parse database URI

### Classes

- `ConnectionManager` - High-level connection manager
- `ConnectionPool` - Thread-safe connection pool
- `Connection` - Managed database connection
- `ConnectionConfig` - Connection configuration
- `ConnectionMetrics` - Performance metrics
- `URIParser` - URI parser
- `ParsedURI` - Parsed URI result

### Enums

- `HeliosDBMode` - Operational modes (REPL, Server, Daemon, Auto)
- `SQLiteOpenMode` - Open modes (ro, rw, rwc)
- `CacheMode` - Cache sharing modes (shared, private)
- `ConnectionState` - Connection states
- `URIScheme` - URI schemes (sqlite, heliosdb, file)

## Examples

Complete examples are available in:
- `examples_connection_wrapper.py` - Integration examples
- `HELIOSDB_SQLITE_CONNECTION_GUIDE.md` - Comprehensive guide
- `test_connection_wrapper.py` - Unit tests

## Documentation

- [Connection Guide](./HELIOSDB_SQLITE_CONNECTION_GUIDE.md) - Comprehensive usage guide
- [API Documentation](./HELIOSDB_SQLITE_CONNECTION_WRAPPER.py) - Full API reference
- [URI Parser Documentation](./HELIOSDB_SQLITE_URI_PARSER.py) - URI parsing details

## License

Same as HeliosDB-Lite (see main repository LICENSE file)

## Support

For issues, questions, or contributions, please visit the main HeliosDB-Lite repository.
