# HeliosDB SQLite Compatibility Layer

**Drop-in replacement for Python's `sqlite3` module with zero code changes required.**

HeliosDB-Lite's SQLite compatibility layer enables existing Python applications to seamlessly use HeliosDB's advanced features (vector search, database branching, time-travel queries, encryption) while maintaining 100% API compatibility with the standard `sqlite3` module.

## Quick Start

### Installation

```bash
# Install from source (for development)
cd sdks/python
pip install -e .

# Or copy heliosdb_sqlite to your project
cp -r heliosdb_sqlite /path/to/your/project/
```

### Basic Usage

```python
# Instead of:
# import sqlite3

# Just change the import:
import heliosdb_sqlite as sqlite3

# Everything else works identically!
conn = sqlite3.connect('myapp.db')
cursor = conn.cursor()
cursor.execute("CREATE TABLE users (id INT, name TEXT)")
cursor.execute("INSERT INTO users VALUES (1, 'Alice')")
cursor.execute("SELECT * FROM users")
for row in cursor:
    print(row)
conn.commit()
conn.close()
```

That's it! Your existing sqlite3 code now runs on HeliosDB-Lite with zero changes.

## Features

### 100% SQLite Compatibility

- ✅ All `Connection` methods (`execute`, `commit`, `rollback`, `cursor`, etc.)
- ✅ All `Cursor` methods (`execute`, `fetchone`, `fetchall`, `executemany`, etc.)
- ✅ `Row` factory for name-based column access
- ✅ Context managers (`with` statements)
- ✅ Transaction control (autocommit, explicit transactions)
- ✅ Parameter binding (positional `?` and named `:name`, `@name`, `$name`)
- ✅ Type adapters and converters
- ✅ Complete exception hierarchy
- ✅ Trace callbacks and introspection

### Advanced HeliosDB Features

Access advanced database capabilities while using the familiar sqlite3 API:

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect('myapp.db')

# Vector search
conn.execute("""
    CREATE TABLE documents (
        id INT,
        content TEXT,
        embedding VECTOR(384)
    )
""")

results = conn.execute_vector_search(
    table='documents',
    column='embedding',
    query_vector=[0.1, 0.2, ...],  # Your embedding
    limit=10,
    metric='cosine'
)

# Database branching
conn.create_branch('test_branch', from_branch='main')

# Time-travel queries
cursor = conn.execute("""
    SELECT * FROM orders
    AS OF TIMESTAMP '2025-12-01 10:00:00'
""")
```

### Multi-Mode Deployment

Choose the deployment mode that fits your needs:

```python
# Embedded mode (default) - Best for scripts, notebooks
conn = sqlite3.connect('myapp.db')

# Daemon mode - Best for production, multi-process
conn = sqlite3.connect(
    'myapp.db',
    mode='daemon',
    server_port=5432
)

# Hybrid mode - Best for development
conn = sqlite3.connect('myapp.db', mode='hybrid')
# Start embedded, switch to daemon when needed
conn.switch_to_server(port=5432)
```

## Architecture

```
Application Code (unchanged)
        ↓
heliosdb_sqlite (sqlite3 API)
        ↓
┌───────────────────────┐
│  Embedded    Daemon   │
│   Mode       Mode     │
└───────────────────────┘
        ↓
HeliosDB-Lite Core
        ↓
RocksDB Storage
```

### How It Works

1. **Application** uses standard sqlite3 API calls
2. **Wrapper** intercepts calls and routes to HeliosDB-Lite
3. **Embedded mode** spawns `heliosdb-nano repl` process per query
4. **Daemon mode** uses PostgreSQL protocol to persistent server
5. **Results** parsed and returned in sqlite3-compatible format

See [HELIOSDB_SQLITE_ARCHITECTURE.md](../HELIOSDB_SQLITE_ARCHITECTURE.md) for detailed architecture.

## API Reference

Complete API documentation available in [HELIOSDB_SQLITE_API_REFERENCE.md](../HELIOSDB_SQLITE_API_REFERENCE.md).

### Core Classes

- **`Connection`** - Database connection
  - `execute()`, `commit()`, `rollback()`, `cursor()`, `close()`
  - `execute_vector_search()`, `create_branch()` (HeliosDB extensions)

- **`Cursor`** - Query execution and result management
  - `execute()`, `fetchone()`, `fetchall()`, `fetchmany()`
  - `executemany()`, `executescript()`

- **`Row`** - Result row with index and name-based access
  - `row[0]`, `row['column_name']`, `row.keys()`

### Exception Hierarchy

```python
Error
├── InterfaceError
└── DatabaseError
    ├── InternalError
    ├── OperationalError
    ├── ProgrammingError
    ├── IntegrityError
    ├── DataError
    └── NotSupportedError
```

## Usage Examples

Complete examples in [HELIOSDB_SQLITE_USAGE_EXAMPLES.py](../HELIOSDB_SQLITE_USAGE_EXAMPLES.py).

### Example 1: Basic Migration

```python
# Before (original sqlite3 code)
import sqlite3
conn = sqlite3.connect('app.db')

# After (just change import!)
import heliosdb_sqlite as sqlite3
conn = sqlite3.connect('app.db')
# Rest of code unchanged
```

### Example 2: Transactions

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect('app.db')

# Context manager (auto-commit/rollback)
with conn:
    conn.execute("INSERT INTO accounts VALUES (1, 100.0)")
    conn.execute("INSERT INTO accounts VALUES (2, 200.0)")
# Committed automatically

# Explicit transaction
try:
    conn.execute("BEGIN")
    conn.execute("UPDATE accounts SET balance = balance - 50 WHERE id = 1")
    conn.execute("UPDATE accounts SET balance = balance + 50 WHERE id = 2")
    conn.execute("COMMIT")
except Exception as e:
    conn.execute("ROLLBACK")
```

### Example 3: Row Factory

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect('app.db')
conn.row_factory = sqlite3.Row

cursor = conn.execute("SELECT id, name, email FROM users")
for row in cursor:
    # Access by name or index
    print(f"{row['name']}: {row['email']}")
    print(f"{row[0]}: {row[1]}")
```

### Example 4: Parameter Binding

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect(':memory:')

# Positional parameters
conn.execute("INSERT INTO users VALUES (?, ?)", (1, 'Alice'))

# Named parameters (multiple styles)
conn.execute("INSERT INTO users VALUES (:id, :name)", {'id': 2, 'name': 'Bob'})
conn.execute("INSERT INTO users VALUES (@id, @name)", {'id': 3, 'name': 'Charlie'})
conn.execute("INSERT INTO users VALUES ($id, $name)", {'id': 4, 'name': 'Diana'})
```

### Example 5: Vector Search

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect('app.db')

# Create vector table
conn.execute("""
    CREATE TABLE embeddings (
        id INT PRIMARY KEY,
        text TEXT,
        vector VECTOR(384)
    )
""")

# Insert vectors
conn.execute(
    "INSERT INTO embeddings VALUES (?, ?, ?::vector)",
    (1, 'example text', '[0.1, 0.2, ...]')
)

# Search similar vectors
cursor = conn.execute("""
    SELECT id, text
    FROM embeddings
    ORDER BY vector <-> '[0.1, 0.2, ...]'::vector
    LIMIT 5
""")
```

## Performance Considerations

### Embedded Mode

**Pros:**
- Zero server management
- Instant startup
- Perfect for scripts, notebooks, single-user apps

**Cons:**
- Process spawn overhead (~10-50ms per query)
- No connection pooling

**Best practices:**
- Use `executemany()` for bulk inserts
- Batch operations in transactions
- Consider hybrid mode for larger workloads

### Daemon Mode

**Pros:**
- Persistent connection (no spawn overhead)
- Production-ready
- Multi-client support
- Better concurrency

**Cons:**
- Server setup required
- Network overhead (minimal for local)

**Best practices:**
- Use for production deployments
- Enable connection pooling
- Monitor server resources

## Testing

Run the comprehensive test suite:

```bash
cd sdks/python
python HELIOSDB_SQLITE_USAGE_EXAMPLES.py
```

This runs 10 example scenarios covering:
1. Drop-in replacement
2. Code migration
3. Advanced features
4. Multi-mode usage
5. Vector search
6. Database branching
7. Time-travel queries
8. Transaction handling
9. Error handling
10. Production patterns

## Requirements

- Python 3.8+
- HeliosDB-Lite binary (`heliosdb-nano`) in PATH
- Optional: `psycopg2` for daemon mode

## Deployment Modes

### Embedded Mode (Default)

```python
conn = sqlite3.connect('myapp.db')
# or explicitly:
conn = sqlite3.connect('myapp.db', mode='embedded')
```

**Use when:**
- Running scripts, notebooks
- Single-user applications
- Development/testing
- No server management desired

### Daemon Mode

```bash
# Start server first
heliosdb-nano start --port 5432 --daemon
```

```python
conn = sqlite3.connect(
    'myapp.db',
    mode='daemon',
    server_port=5432
)
```

**Use when:**
- Production deployments
- Multi-process applications
- Need connection pooling
- High concurrency required

### Hybrid Mode

```python
conn = sqlite3.connect('myapp.db', mode='hybrid')

# Start in embedded mode
cursor = conn.execute("SELECT * FROM users")

# Switch to daemon when needed
conn.switch_to_server(port=5432)
```

**Use when:**
- Development with production testing
- Gradual migration to daemon
- Flexible deployment requirements

## Migration Guide

### From SQLite to HeliosDB

**Step 1:** Install heliosdb_sqlite
```bash
pip install heliosdb-sqlite
```

**Step 2:** Change import
```python
# Before
import sqlite3

# After
import heliosdb_sqlite as sqlite3
```

**Step 3:** Done! No other changes needed.

### Testing Migration

1. Run existing test suite with heliosdb_sqlite
2. Verify all tests pass
3. Check for any unsupported features (very rare)
4. Test advanced features (vectors, branching) if needed

### Rollback Plan

Keep original import as fallback:
```python
try:
    import heliosdb_sqlite as sqlite3
except ImportError:
    import sqlite3  # Fallback to standard sqlite3
```

## Limitations

### Current Limitations

1. **User-Defined Functions (UDFs)**: Not yet supported in embedded mode
2. **Extensions**: SQLite C extensions cannot be loaded
3. **Savepoints**: Basic support (may vary by mode)

### Unsupported Features

- Custom collations
- Virtual tables (via extensions)
- FTS5 (use HeliosDB's native vector search instead)

### Workarounds

Most limitations have HeliosDB-native alternatives:

| SQLite Feature | HeliosDB Alternative |
|----------------|---------------------|
| FTS5 | Vector search with embeddings |
| JSON1 | Native JSONB support |
| R*Tree | HNSW vector index |
| UDFs | SQL functions or Python preprocessing |

## Troubleshooting

### Common Issues

**Issue:** `heliosdb-nano: command not found`

**Solution:** Install HeliosDB-Lite and ensure it's in PATH
```bash
export PATH="/path/to/heliosdb-nano:$PATH"
```

---

**Issue:** Connection timeout in embedded mode

**Solution:** Increase timeout parameter
```python
conn = sqlite3.connect('myapp.db', timeout=30.0)
```

---

**Issue:** Daemon mode connection failed

**Solution:** Ensure server is running
```bash
heliosdb-nano status
# If not running:
heliosdb-nano start --port 5432 --daemon
```

---

**Issue:** Parameter binding not working

**Solution:** Ensure proper parameter style
```python
# Correct
cursor.execute("SELECT * FROM users WHERE id = ?", (1,))

# Wrong (missing tuple)
cursor.execute("SELECT * FROM users WHERE id = ?", 1)
```

## Contributing

Contributions welcome! Please see main HeliosDB-Lite repository for guidelines.

### Development Setup

```bash
cd sdks/python
pip install -e .[dev]
python -m pytest tests/
```

### Testing

```bash
# Run all examples
python HELIOSDB_SQLITE_USAGE_EXAMPLES.py

# Run specific example
python -c "from HELIOSDB_SQLITE_USAGE_EXAMPLES import example_1_drop_in_replacement; example_1_drop_in_replacement()"

# Integration tests (requires HeliosDB-Lite)
python -m pytest tests/integration/
```

## License

MIT License - see main HeliosDB-Lite repository

## Support

- **Documentation:** See docs/ directory
- **Issues:** GitHub Issues
- **Examples:** HELIOSDB_SQLITE_USAGE_EXAMPLES.py
- **API Reference:** HELIOSDB_SQLITE_API_REFERENCE.md

## Roadmap

Future enhancements:

1. **Direct Rust binding** via PyO3 (eliminate subprocess overhead)
2. **Async support** with `asyncio` compatible API
3. **Connection pooling** built-in for daemon mode
4. **Streaming results** for large queries
5. **UDF support** (register Python functions as SQL UDFs)
6. **Query caching** for improved performance

## Summary

The HeliosDB SQLite Compatibility Layer provides:

✅ **100% API Compatibility** - Drop-in replacement for sqlite3
✅ **Zero Code Changes** - Change only the import statement
✅ **Advanced Features** - Vector search, branching, time-travel
✅ **Flexible Deployment** - Embedded, daemon, or hybrid modes
✅ **Production-Ready** - Transactions, error handling, thread safety

Get started in 30 seconds:
```python
import heliosdb_sqlite as sqlite3
conn = sqlite3.connect('myapp.db')
```

That's it! You now have access to HeliosDB's advanced capabilities while keeping your existing code unchanged.
