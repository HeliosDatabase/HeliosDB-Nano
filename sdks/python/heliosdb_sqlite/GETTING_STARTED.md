# Getting Started with HeliosDB SQLite Compatibility Layer

This guide will help you get started with the HeliosDB SQLite compatibility layer in under 5 minutes.

## Installation

### Option 1: Install from PyPI (when published)
```bash
pip install heliosdb-sqlite
```

### Option 2: Install from Source
```bash
cd /home/claude/HeliosDB-Lite/sdks/python
pip install -e .
```

### Option 3: Copy Module
```bash
cp -r heliosdb_sqlite /path/to/your/project/
```

## Prerequisites

1. **Python 3.8+** installed
2. **HeliosDB-Lite binary** in your PATH

```bash
# Verify HeliosDB-Lite is installed
heliosdb-lite --version

# If not, add to PATH
export PATH="/path/to/heliosdb-lite:$PATH"
```

## 30-Second Quick Start

```python
# Change this ONE line in your code:
# import sqlite3
import heliosdb_sqlite as sqlite3

# Everything else works exactly the same!
conn = sqlite3.connect('myapp.db')
cursor = conn.cursor()
cursor.execute("SELECT 'Hello from HeliosDB!'")
print(cursor.fetchone()[0])
conn.close()
```

## 5-Minute Tutorial

### Step 1: Basic Operations

```python
import heliosdb_sqlite as sqlite3

# Connect (creates/opens database)
conn = sqlite3.connect(':memory:')  # In-memory for testing
# or: conn = sqlite3.connect('myapp.db')  # File-based

# Create table
conn.execute("""
    CREATE TABLE employees (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        department TEXT,
        salary REAL
    )
""")

# Insert data
conn.execute("INSERT INTO employees VALUES (1, 'Alice', 'Engineering', 120000)")

# Or use parameters (safer!)
conn.execute(
    "INSERT INTO employees VALUES (?, ?, ?, ?)",
    (2, 'Bob', 'Sales', 95000)
)

# Commit changes
conn.commit()

# Query data
cursor = conn.execute("SELECT * FROM employees")
for row in cursor:
    print(f"{row[1]} ({row[2]}): ${row[3]:,.2f}")

conn.close()
```

### Step 2: Advanced Features

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect(':memory:')

# Use Row factory for name-based access
conn.row_factory = sqlite3.Row

conn.execute("CREATE TABLE users (id INT, name TEXT, email TEXT)")
conn.execute("INSERT INTO users VALUES (1, 'Alice', 'alice@example.com')")

cursor = conn.execute("SELECT * FROM users")
row = cursor.fetchone()

# Access by name OR index
print(row['name'])      # 'Alice'
print(row['email'])     # 'alice@example.com'
print(row[0])           # 1

# Context manager (auto-commit/rollback)
with conn:
    conn.execute("INSERT INTO users VALUES (2, 'Bob', 'bob@example.com')")
# Auto-commits on success

conn.close()
```

### Step 3: HeliosDB-Specific Features

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect(':memory:')

# Vector search
conn.execute("""
    CREATE TABLE documents (
        id INT PRIMARY KEY,
        content TEXT,
        embedding VECTOR(3)
    )
""")

conn.execute(
    "INSERT INTO documents VALUES (1, 'AI tutorial', '[0.1, 0.2, 0.3]'::vector)"
)

# Query similar vectors
cursor = conn.execute("""
    SELECT id, content
    FROM documents
    ORDER BY embedding <-> '[0.11, 0.21, 0.31]'::vector
    LIMIT 5
""")

for row in cursor:
    print(f"Document: {row[1]}")

conn.close()
```

## Common Patterns

### Pattern 1: Transactions

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect('app.db')

try:
    with conn:  # Auto-commit/rollback
        conn.execute("INSERT INTO accounts VALUES (1, 1000)")
        conn.execute("INSERT INTO accounts VALUES (2, 500)")
    print("Transaction committed!")
except sqlite3.IntegrityError as e:
    print(f"Transaction failed: {e}")
```

### Pattern 2: Bulk Inserts

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect('app.db')
conn.execute("CREATE TABLE users (id INT, name TEXT)")

# Efficient bulk insert
users = [
    (1, 'Alice'),
    (2, 'Bob'),
    (3, 'Charlie'),
]

conn.executemany("INSERT INTO users VALUES (?, ?)", users)
conn.commit()
```

### Pattern 3: Parameter Binding

```python
import heliosdb_sqlite as sqlite3

conn = sqlite3.connect('app.db')

# Positional parameters (?)
user_id = 1
cursor = conn.execute("SELECT * FROM users WHERE id = ?", (user_id,))

# Named parameters (:name)
params = {'status': 'active', 'min_age': 18}
cursor = conn.execute(
    "SELECT * FROM users WHERE status = :status AND age > :min_age",
    params
)
```

## Choosing a Deployment Mode

### Embedded Mode (Default)
**Best for:** Scripts, notebooks, single-user apps

```python
conn = sqlite3.connect('app.db')
# Simple, no server required
```

**Pros:**
- Zero configuration
- Instant startup
- Perfect for development

**Cons:**
- Process spawn overhead per query
- No multi-client support

### Daemon Mode
**Best for:** Production, multi-process apps

```bash
# Start server first
heliosdb-lite start --port 5432 --daemon
```

```python
conn = sqlite3.connect(
    'app.db',
    mode='daemon',
    server_port=5432
)
```

**Pros:**
- Persistent connection
- Multi-client support
- Production-ready

**Cons:**
- Server management required

### Hybrid Mode
**Best for:** Development with flexible deployment

```python
conn = sqlite3.connect('app.db', mode='hybrid')

# Start in embedded mode
cursor = conn.execute("SELECT * FROM users")

# Switch to daemon when needed
conn.switch_to_server(port=5432)
```

## Troubleshooting

### "heliosdb-lite: command not found"

**Solution:**
```bash
# Find heliosdb-lite
which heliosdb-lite

# Add to PATH
export PATH="/path/to/heliosdb-lite:$PATH"

# Or install
cargo install heliosdb-lite  # If from source
```

### "Connection timeout"

**Solution:**
```python
# Increase timeout
conn = sqlite3.connect('app.db', timeout=30.0)
```

### "Daemon connection failed"

**Solution:**
```bash
# Check server status
heliosdb-lite status

# Start server if needed
heliosdb-lite start --port 5432 --daemon
```

## Next Steps

1. **Read the full documentation:**
   - [README](README.md) - Complete usage guide
   - [Architecture](../HELIOSDB_SQLITE_ARCHITECTURE.md) - How it works
   - [API Reference](../HELIOSDB_SQLITE_API_REFERENCE.md) - All methods

2. **Run the examples:**
   ```bash
   python ../HELIOSDB_SQLITE_USAGE_EXAMPLES.py
   ```

3. **Migrate your app:**
   - Change `import sqlite3` to `import heliosdb_sqlite as sqlite3`
   - Test your application
   - Explore advanced features (vectors, branching, time-travel)

4. **Join the community:**
   - GitHub Issues for questions
   - Contribute examples
   - Report bugs

## Summary

You now know how to:
- ✅ Install heliosdb_sqlite
- ✅ Replace sqlite3 with zero code changes
- ✅ Use basic database operations
- ✅ Access advanced features
- ✅ Choose the right deployment mode
- ✅ Troubleshoot common issues

Welcome to HeliosDB!
