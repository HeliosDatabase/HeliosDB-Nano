# heliosdb-sqlite

[![PyPI Version](https://img.shields.io/pypi/v/heliosdb-sqlite.svg)](https://pypi.org/project/heliosdb-sqlite/)
[![Python Versions](https://img.shields.io/pypi/pyversions/heliosdb-sqlite.svg)](https://pypi.org/project/heliosdb-sqlite/)
[![License](https://img.shields.io/pypi/l/heliosdb-sqlite.svg)](https://github.com/heliosdb/heliosdb-nano/blob/main/LICENSE)
[![Downloads](https://img.shields.io/pypi/dm/heliosdb-sqlite.svg)](https://pypi.org/project/heliosdb-sqlite/)

**SQLite-compatible interface for HeliosDB-Lite** - A drop-in replacement for Python's `sqlite3` module with enhanced features including vector search, encryption, and time-travel queries.

---

## Features

- **100% SQLite API Compatibility** - Drop-in replacement for Python's `sqlite3` module
- **Vector Search** - Built-in vector search with Product Quantization (8-16x compression)
- **Transparent Encryption** - AES-256-GCM encryption with <3% overhead
- **Time-Travel Queries** - Access historical data with `AS OF TIMESTAMP`
- **Database Branching** - Git-like workflows for schema changes
- **PostgreSQL Types** - Extended type support (JSONB, UUID, VECTOR)
- **Zero Dependencies** - Pure Python with bundled HeliosDB-Lite binary
- **Cross-Platform** - Linux, macOS, Windows support

---

## Installation

### Standard Installation

```bash
pip install heliosdb-sqlite
```

### With Optional Dependencies

```bash
# Vector operations (numpy, scipy)
pip install heliosdb-sqlite[vector]

# Pandas integration
pip install heliosdb-sqlite[pandas]

# All features
pip install heliosdb-sqlite[all]
```

### Verify Installation

```bash
python -c "import heliosdb_sqlite; print(heliosdb_sqlite.__version__)"
# Output: 3.0.0

# Run comprehensive tests
python -m heliosdb_sqlite.cli check
```

---

## Quick Start

### Drop-in Replacement for sqlite3

```python
# Change this:
# import sqlite3
# To this:
import heliosdb_sqlite as sqlite3

# Everything else works exactly the same!
conn = sqlite3.connect(':memory:')
cursor = conn.cursor()

cursor.execute('CREATE TABLE users (id INTEGER, name TEXT)')
cursor.execute('INSERT INTO users VALUES (?, ?)', (1, 'Alice'))
conn.commit()

cursor.execute('SELECT * FROM users')
print(cursor.fetchall())  # [(1, 'Alice')]

conn.close()
```

### Vector Search Example

```python
import heliosdb_sqlite as db

conn = db.connect('vectors.db')
cursor = conn.cursor()

# Create table with vector column
cursor.execute('''
    CREATE TABLE documents (
        id INTEGER PRIMARY KEY,
        title TEXT,
        embedding VECTOR(768)
    )
''')

# Insert document with vector
import numpy as np
embedding = np.random.rand(768).tolist()
cursor.execute(
    'INSERT INTO documents VALUES (?, ?, ?)',
    (1, 'HeliosDB Guide', embedding)
)
conn.commit()

# Semantic search (k-NN)
query_vector = np.random.rand(768).tolist()
cursor.execute('''
    SELECT id, title, embedding <=> ? AS distance
    FROM documents
    ORDER BY distance
    LIMIT 5
''', (query_vector,))

for row in cursor.fetchall():
    print(f"Document {row[0]}: {row[1]} (distance: {row[2]:.4f})")

conn.close()
```

### Time-Travel Queries

```python
import heliosdb_sqlite as db

conn = db.connect('audit.db')
cursor = conn.cursor()

# Query historical data
cursor.execute('''
    SELECT * FROM orders
    AS OF TIMESTAMP '2025-01-01 12:00:00'
    WHERE customer_id = ?
''', (123,))

historical_orders = cursor.fetchall()
print(f"Orders as of 2025-01-01: {historical_orders}")

conn.close()
```

### Encrypted Database

```python
import heliosdb_sqlite as db
import os

# Set encryption key
os.environ['HELIOSDB_ENCRYPTION_KEY'] = 'your-32-byte-key-here'

# Database is automatically encrypted
conn = db.connect('encrypted.db')
cursor = conn.cursor()

cursor.execute('CREATE TABLE secrets (id INTEGER, data TEXT)')
cursor.execute('INSERT INTO secrets VALUES (?, ?)', (1, 'confidential'))
conn.commit()
conn.close()

# Data is encrypted at rest with AES-256-GCM
```

---

## API Reference

### Connection Methods

```python
import heliosdb_sqlite

# Create connection
conn = heliosdb_sqlite.connect(
    database='mydb.db',           # ':memory:' for in-memory
    timeout=5.0,                  # Lock timeout in seconds
    isolation_level='DEFERRED',   # Transaction isolation
    check_same_thread=True        # Thread safety check
)

# Execute queries
cursor = conn.cursor()
conn.commit()
conn.rollback()
conn.close()

# Context manager (auto-commit/rollback)
with heliosdb_sqlite.connect(':memory:') as conn:
    cursor = conn.cursor()
    cursor.execute('CREATE TABLE test (id INTEGER)')
```

### Cursor Methods

```python
cursor = conn.cursor()

# Execute single statement
cursor.execute('SELECT * FROM users WHERE id = ?', (1,))

# Execute with named parameters
cursor.execute('SELECT * FROM users WHERE name = :name', {'name': 'Alice'})

# Execute many (batch insert)
cursor.executemany(
    'INSERT INTO users VALUES (?, ?)',
    [(1, 'Alice'), (2, 'Bob'), (3, 'Charlie')]
)

# Execute script (multiple statements)
cursor.executescript('''
    CREATE TABLE users (id INTEGER, name TEXT);
    CREATE INDEX idx_name ON users(name);
''')

# Fetch results
row = cursor.fetchone()           # Single row
rows = cursor.fetchmany(10)       # Multiple rows
all_rows = cursor.fetchall()      # All remaining rows

# Iterate over results
for row in cursor:
    print(row)
```

### Exception Handling

```python
import heliosdb_sqlite

try:
    conn = heliosdb_sqlite.connect('mydb.db')
    cursor = conn.cursor()
    cursor.execute('SELECT * FROM nonexistent_table')

except heliosdb_sqlite.Error as e:
    print(f"Database error: {e}")

except heliosdb_sqlite.OperationalError as e:
    print(f"Operational error: {e}")

except heliosdb_sqlite.ProgrammingError as e:
    print(f"Programming error: {e}")

finally:
    if 'conn' in locals():
        conn.close()
```

---

## Platform Support

| Platform | Architecture | Status | Wheel Available |
|----------|--------------|--------|-----------------|
| **Linux** | x86_64 | ✅ Stable | ✅ manylinux2014 |
| **Linux** | aarch64 | ✅ Stable | ✅ manylinux2014 |
| **macOS** | x86_64 (Intel) | ✅ Stable | ✅ 10.12+ |
| **macOS** | arm64 (Apple Silicon) | ✅ Stable | ✅ 11.0+ |
| **Windows** | x86_64 | ✅ Stable | ✅ Win10+ |

### Python Version Support

- ✅ Python 3.8
- ✅ Python 3.9
- ✅ Python 3.10
- ✅ Python 3.11
- ✅ Python 3.12
- ✅ Python 3.13

---

## Performance

### Compression Ratios

| Data Type | Compression | Memory Savings |
|-----------|-------------|----------------|
| **768-dim vectors** | 384x | 3,072 bytes → 8 bytes |
| **512-dim vectors** | 256x | 2,048 bytes → 8 bytes |

### Query Performance

| Operation | Performance | Notes |
|-----------|-------------|-------|
| **Vector search** | 1K-5K QPS | With PQ compression |
| **Full table scan** | 500K-1M rows/sec | SIMD-accelerated |
| **Time-travel lookup** | <100ms | Indexed snapshots |

---

## Comparison with sqlite3

| Feature | sqlite3 | heliosdb-sqlite |
|---------|---------|-----------------|
| **SQLite API** | ✅ | ✅ 100% compatible |
| **Vector Search** | ❌ | ✅ Built-in HNSW + PQ |
| **Encryption** | ❌ | ✅ AES-256-GCM |
| **Time-Travel** | ❌ | ✅ AS OF queries |
| **Branching** | ❌ | ✅ Git-like workflows |
| **PostgreSQL Types** | ❌ | ✅ JSONB, UUID, VECTOR |
| **Performance** | Fast | Fast + SIMD |

---

## Documentation

- **Full Documentation**: https://docs.heliosdb.io/sqlite-compat
- **API Reference**: https://docs.heliosdb.io/sqlite-compat/api
- **Examples**: https://github.com/heliosdb/heliosdb-nano/tree/main/examples
- **HeliosDB-Lite Docs**: https://github.com/heliosdb/heliosdb-nano

---

## Troubleshooting

### Binary Not Found

```bash
# Reinstall package
pip install --force-reinstall --no-cache-dir heliosdb-sqlite

# Check binary location
python -c "import heliosdb_sqlite; print(heliosdb_sqlite.get_binary_path())"
```

### Permission Denied (Unix/macOS)

```bash
# Make binary executable
python -c "import heliosdb_sqlite; import os; os.chmod(heliosdb_sqlite.get_binary_path(), 0o755)"
```

### Platform Not Supported

If your platform doesn't have pre-built wheels, you can build from source:

```bash
# Install Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build from source
pip install heliosdb-sqlite --no-binary heliosdb-sqlite
```

---

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](https://github.com/heliosdb/heliosdb-nano/blob/main/CONTRIBUTING.md) for guidelines.

### Development Setup

```bash
git clone https://github.com/heliosdb/heliosdb-nano.git
cd heliosdb-nano/sdks/python-sqlite

# Install in development mode
pip install -e ".[dev]"

# Run tests
pytest tests/ -v

# Format code
black heliosdb_sqlite/ tests/
ruff check heliosdb_sqlite/ tests/

# Type checking
mypy heliosdb_sqlite/
```

---

## License

Apache License 2.0 - see [LICENSE](https://github.com/heliosdb/heliosdb-nano/blob/main/LICENSE) for details.

---

## Support

- **GitHub Issues**: https://github.com/heliosdb/heliosdb-nano/issues
- **Discussions**: https://github.com/heliosdb/heliosdb-nano/discussions
- **Discord**: https://discord.gg/heliosdb
- **Email**: support@heliosdb.io

---

## Acknowledgments

Built on top of:
- **HeliosDB-Lite** - PostgreSQL-compatible embedded database
- **RocksDB** - High-performance key-value store
- **Apache Arrow** - Columnar data format
- **HNSW** - Approximate nearest neighbor search

---

**Made with ❤️ for developers who need production-grade embedded databases**

[⭐ Star us on GitHub](https://github.com/heliosdb/heliosdb-nano)
