# HeliosDB SQLite API Reference

Complete API documentation for the HeliosDB SQLite compatibility layer. This module provides 100% compatibility with Python's `sqlite3` module while adding HeliosDB-specific extensions.

## Module Constants

### Version Information

```python
heliosdb_sqlite.version
```
Version string of the heliosdb_sqlite module.
- Type: `str`
- Example: `"3.0.1"`

```python
heliosdb_sqlite.version_info
```
Version tuple of the heliosdb_sqlite module.
- Type: `tuple[int, int, int]`
- Example: `(3, 0, 1)`

```python
heliosdb_sqlite.sqlite_version
```
Version string of the underlying database (HeliosDB-Lite compatible).
- Type: `str`
- Example: `"3.45.0 (HeliosDB-Lite compatible)"`

```python
heliosdb_sqlite.sqlite_version_info
```
Version tuple of the underlying database.
- Type: `tuple[int, int, int]`
- Example: `(3, 45, 0)`

### Parse Constants

```python
heliosdb_sqlite.PARSE_DECLTYPES
```
Flag to parse declared column types.
- Value: `1`
- Use with `detect_types` parameter

```python
heliosdb_sqlite.PARSE_COLNAMES
```
Flag to parse column names for type information.
- Value: `2`
- Use with `detect_types` parameter

### Return Codes

```python
heliosdb_sqlite.SQLITE_OK
```
Successful result.
- Value: `0`

```python
heliosdb_sqlite.SQLITE_ERROR
```
Generic error.
- Value: `1`

```python
heliosdb_sqlite.SQLITE_DENY
```
Access denied (authorizer callback).
- Value: `1`

```python
heliosdb_sqlite.SQLITE_IGNORE
```
Ignore operation (authorizer callback).
- Value: `2`

## Core Functions

### connect()

```python
heliosdb_sqlite.connect(
    database: str,
    timeout: float = 5.0,
    detect_types: int = 0,
    isolation_level: Optional[str] = "DEFERRED",
    check_same_thread: bool = True,
    factory: Optional[type] = None,
    cached_statements: int = 128,
    uri: bool = False,
    **kwargs
) -> Connection
```

Open a connection to a HeliosDB-Lite database.

**Parameters:**
- `database` (str): Database file path, or `:memory:` for in-memory database
- `timeout` (float): Connection timeout in seconds (default: 5.0)
- `detect_types` (int): Type detection flags (`PARSE_DECLTYPES | PARSE_COLNAMES`)
- `isolation_level` (str | None): Transaction isolation level
  - `"DEFERRED"` (default): Lock on first write
  - `"IMMEDIATE"`: Lock immediately
  - `"EXCLUSIVE"`: Exclusive lock
  - `None`: Autocommit mode
- `check_same_thread` (bool): Enforce single-thread usage (default: True)
- `factory` (type | None): Custom Connection class
- `cached_statements` (int): Statement cache size (default: 128)
- `uri` (bool): Treat database as URI (default: False)
- `**kwargs`: HeliosDB-specific options:
  - `mode` (str): Connection mode (`'embedded'`, `'daemon'`, `'hybrid'`)
  - `data_dir` (str): Custom data directory path
  - `server_port` (int): Server port for daemon mode (default: 5432)
  - `server_host` (str): Server host for daemon mode (default: '127.0.0.1')

**Returns:**
- `Connection`: Database connection object

**Examples:**
```python
# In-memory database
conn = heliosdb_sqlite.connect(':memory:')

# File-based database
conn = heliosdb_sqlite.connect('myapp.db')

# Daemon mode
conn = heliosdb_sqlite.connect('myapp.db', mode='daemon', server_port=5432)

# Hybrid mode with custom data directory
conn = heliosdb_sqlite.connect('myapp.db', mode='hybrid', data_dir='/var/lib/heliosdb')

# Type detection enabled
conn = heliosdb_sqlite.connect('myapp.db', detect_types=PARSE_DECLTYPES | PARSE_COLNAMES)

# Autocommit mode
conn = heliosdb_sqlite.connect('myapp.db', isolation_level=None)
```

### register_adapter()

```python
heliosdb_sqlite.register_adapter(type_: type, callable_: Callable[[Any], Any]) -> None
```

Register a callable to convert Python type to SQL-compatible type.

**Parameters:**
- `type_` (type): Python type to adapt
- `callable_` (Callable): Function that converts type to SQL-compatible value

**Examples:**
```python
import datetime

def adapt_date(val):
    return val.isoformat()

heliosdb_sqlite.register_adapter(datetime.date, adapt_date)

# Now dates are automatically converted
conn.execute("INSERT INTO events (date) VALUES (?)", (datetime.date.today(),))
```

### register_converter()

```python
heliosdb_sqlite.register_converter(typename: str, callable_: Callable[[bytes], Any]) -> None
```

Register a callable to convert SQL type to Python type.

**Parameters:**
- `typename` (str): SQL type name (case-insensitive)
- `callable_` (Callable): Function that converts SQL value to Python object

**Examples:**
```python
import datetime

def convert_date(val):
    return datetime.date.fromisoformat(val.decode('utf-8'))

heliosdb_sqlite.register_converter("date", convert_date)

# Enable converter via detect_types
conn = heliosdb_sqlite.connect('myapp.db', detect_types=PARSE_DECLTYPES)
```

### register_trace_callback()

```python
heliosdb_sqlite.register_trace_callback(callback: Optional[Callable[[str], None]]) -> None
```

Register a global callback for SQL statement tracing.

**Parameters:**
- `callback` (Callable | None): Function called for each SQL statement, or None to disable

**Examples:**
```python
def trace_sql(statement):
    print(f"[SQL] {statement}")

heliosdb_sqlite.register_trace_callback(trace_sql)

conn = heliosdb_sqlite.connect(':memory:')
conn.execute("CREATE TABLE test (id INT)")  # Prints: [SQL] CREATE TABLE test (id INT)
```

### enable_callback_tracebacks()

```python
heliosdb_sqlite.enable_callback_tracebacks(flag: bool) -> None
```

Enable or disable traceback printing for callback exceptions.

**Parameters:**
- `flag` (bool): True to enable, False to disable

### complete_statement()

```python
heliosdb_sqlite.complete_statement(statement: str) -> bool
```

Check if an SQL statement is complete (properly terminated).

**Parameters:**
- `statement` (str): SQL statement to check

**Returns:**
- `bool`: True if statement ends with semicolon

**Examples:**
```python
complete_statement("SELECT * FROM users")  # False
complete_statement("SELECT * FROM users;")  # True
```

## Connection Class

### Constructor

```python
Connection(
    database: str,
    timeout: float = 5.0,
    detect_types: int = 0,
    isolation_level: Optional[str] = "DEFERRED",
    check_same_thread: bool = True,
    factory: Optional[type] = None,
    cached_statements: int = 128,
    uri: bool = False,
    **kwargs
)
```

Use `connect()` function instead of direct instantiation.

### Attributes

```python
Connection.isolation_level
```
Current transaction isolation level.
- Type: `str | None`
- Writable: Yes

```python
Connection.row_factory
```
Row factory function or class.
- Type: `Callable | None`
- Default: `None` (returns tuples)
- Example: `conn.row_factory = heliosdb_sqlite.Row`

```python
Connection.database
```
Database file path.
- Type: `str`
- Read-only

### Methods

#### cursor()

```python
Connection.cursor(factory: Optional[type] = None) -> Cursor
```

Create a new cursor object.

**Parameters:**
- `factory` (type | None): Custom cursor class

**Returns:**
- `Cursor`: New cursor instance

**Examples:**
```python
cursor = conn.cursor()
cursor.execute("SELECT * FROM users")
```

#### execute()

```python
Connection.execute(
    sql: str,
    parameters: Union[Tuple, Dict] = ()
) -> Cursor
```

Shortcut to create cursor and execute SQL.

**Parameters:**
- `sql` (str): SQL statement
- `parameters` (tuple | dict): Parameter values

**Returns:**
- `Cursor`: Cursor with results

**Examples:**
```python
# Positional parameters
cursor = conn.execute("SELECT * FROM users WHERE id = ?", (1,))

# Named parameters
cursor = conn.execute("SELECT * FROM users WHERE id = :id", {'id': 1})

# No parameters
cursor = conn.execute("SELECT * FROM users")
```

#### executemany()

```python
Connection.executemany(
    sql: str,
    seq_of_parameters: List[Union[Tuple, Dict]]
) -> Cursor
```

Execute SQL for multiple parameter sets.

**Parameters:**
- `sql` (str): SQL statement
- `seq_of_parameters` (list): List of parameter tuples/dicts

**Returns:**
- `Cursor`: Cursor after all executions

**Examples:**
```python
users = [(1, 'Alice'), (2, 'Bob'), (3, 'Charlie')]
conn.executemany("INSERT INTO users VALUES (?, ?)", users)
```

#### executescript()

```python
Connection.executescript(sql_script: str) -> Cursor
```

Execute multiple SQL statements from a script.

**Parameters:**
- `sql_script` (str): SQL script with semicolon-separated statements

**Returns:**
- `Cursor`: Cursor after all executions

**Examples:**
```python
conn.executescript("""
    CREATE TABLE users (id INT, name TEXT);
    INSERT INTO users VALUES (1, 'Alice');
    INSERT INTO users VALUES (2, 'Bob');
""")
```

#### commit()

```python
Connection.commit() -> None
```

Commit current transaction.

**Examples:**
```python
conn.execute("INSERT INTO users VALUES (1, 'Alice')")
conn.commit()
```

#### rollback()

```python
Connection.rollback() -> None
```

Rollback current transaction.

**Examples:**
```python
try:
    conn.execute("INSERT INTO users VALUES (1, 'Alice')")
    conn.execute("INSERT INTO users VALUES (1, 'Bob')")  # Error: duplicate
    conn.commit()
except heliosdb_sqlite.IntegrityError:
    conn.rollback()
```

#### begin()

```python
Connection.begin() -> None
```

Explicitly begin a transaction.

**Examples:**
```python
conn.begin()
conn.execute("INSERT INTO users VALUES (1, 'Alice')")
conn.commit()
```

#### close()

```python
Connection.close() -> None
```

Close the database connection.

**Examples:**
```python
conn.close()
```

#### interrupt()

```python
Connection.interrupt() -> None
```

Interrupt a long-running query.

**Examples:**
```python
import threading

def long_query():
    conn.execute("SELECT * FROM huge_table WHERE expensive_function(x)")

t = threading.Thread(target=long_query)
t.start()
time.sleep(1)
conn.interrupt()  # Cancel the query
```

#### iterdump()

```python
Connection.iterdump() -> Iterator[str]
```

Iterate over SQL dump of database schema.

**Returns:**
- `Iterator[str]`: SQL statements

**Examples:**
```python
for line in conn.iterdump():
    print(line)
```

#### backup()

```python
Connection.backup(
    target: Connection,
    pages: int = -1,
    progress: Optional[Callable[[int, int, int], None]] = None
) -> None
```

Backup database to target connection.

**Parameters:**
- `target` (Connection): Target database connection
- `pages` (int): Pages to copy (-1 for all)
- `progress` (Callable | None): Progress callback

**Examples:**
```python
source = heliosdb_sqlite.connect('main.db')
target = heliosdb_sqlite.connect('backup.db')
source.backup(target)
```

#### Context Manager

```python
with heliosdb_sqlite.connect('myapp.db') as conn:
    conn.execute("INSERT INTO users VALUES (1, 'Alice')")
    # Auto-commit on success, rollback on exception
```

### HeliosDB Extensions

#### switch_to_server()

```python
Connection.switch_to_server(port: int = 5432) -> None
```

Switch from embedded mode to daemon mode (hybrid mode only).

**Parameters:**
- `port` (int): Server port (default: 5432)

**Examples:**
```python
conn = heliosdb_sqlite.connect('myapp.db', mode='hybrid')
# Initially embedded mode
conn.execute("CREATE TABLE test (id INT)")
# Switch to daemon
conn.switch_to_server(port=5432)
# Now uses PostgreSQL protocol
```

#### execute_vector_search()

```python
Connection.execute_vector_search(
    table: str,
    column: str,
    query_vector: List[float],
    limit: int = 10,
    metric: str = 'cosine'
) -> List[Tuple]
```

Execute vector similarity search.

**Parameters:**
- `table` (str): Table name
- `column` (str): Vector column name
- `query_vector` (list[float]): Query vector
- `limit` (int): Number of results (default: 10)
- `metric` (str): Distance metric (`'cosine'`, `'l2'`, `'inner_product'`)

**Returns:**
- `list[tuple]`: Result rows

**Examples:**
```python
# Search for similar embeddings
results = conn.execute_vector_search(
    table='documents',
    column='embedding',
    query_vector=[0.1, 0.2, 0.3, ...],
    limit=5,
    metric='cosine'
)
```

#### create_branch()

```python
Connection.create_branch(branch_name: str, from_branch: str = 'main') -> None
```

Create database branch.

**Parameters:**
- `branch_name` (str): New branch name
- `from_branch` (str): Source branch (default: 'main')

**Examples:**
```python
conn.create_branch('test_branch')
```

## Cursor Class

### Attributes

```python
Cursor.description
```
Column metadata for last query.
- Type: `list[tuple] | None`
- Format: `[(name, None, None, None, None, None, None), ...]`
- Read-only

```python
Cursor.rowcount
```
Number of rows affected/returned.
- Type: `int`
- `-1` if unknown
- Read-only

```python
Cursor.lastrowid
```
Row ID of last inserted row.
- Type: `int | None`
- Read-only

```python
Cursor.arraysize
```
Default number of rows for `fetchmany()`.
- Type: `int`
- Default: `1`
- Writable

```python
Cursor.connection
```
Parent connection object.
- Type: `Connection`
- Read-only

```python
Cursor.row_factory
```
Row factory for this cursor (overrides connection factory).
- Type: `Callable | None`
- Writable

### Methods

#### execute()

```python
Cursor.execute(
    sql: str,
    parameters: Union[Tuple, Dict] = ()
) -> Cursor
```

Execute SQL statement.

**Parameters:**
- `sql` (str): SQL statement
- `parameters` (tuple | dict): Parameter values

**Returns:**
- `Cursor`: Self for chaining

**Parameter Binding:**

Positional (`?`):
```python
cursor.execute("SELECT * FROM users WHERE id = ?", (1,))
cursor.execute("INSERT INTO users VALUES (?, ?)", (1, 'Alice'))
```

Named (`:name`, `@name`, `$name`):
```python
cursor.execute("SELECT * FROM users WHERE id = :id", {'id': 1})
cursor.execute("SELECT * FROM users WHERE id = @id", {'id': 1})
cursor.execute("SELECT * FROM users WHERE id = $id", {'id': 1})
```

#### executemany()

```python
Cursor.executemany(
    sql: str,
    seq_of_parameters: List[Union[Tuple, Dict]]
) -> Cursor
```

Execute SQL for multiple parameter sets.

**Examples:**
```python
data = [(1, 'Alice'), (2, 'Bob'), (3, 'Charlie')]
cursor.executemany("INSERT INTO users VALUES (?, ?)", data)
```

#### executescript()

```python
Cursor.executescript(sql_script: str) -> Cursor
```

Execute SQL script with multiple statements.

**Examples:**
```python
cursor.executescript("""
    DROP TABLE IF EXISTS users;
    CREATE TABLE users (id INT, name TEXT);
    INSERT INTO users VALUES (1, 'Alice');
""")
```

#### fetchone()

```python
Cursor.fetchone() -> Optional[Union[Tuple, Row]]
```

Fetch next row from results.

**Returns:**
- `tuple | Row | None`: Next row, or None if exhausted

**Examples:**
```python
cursor.execute("SELECT * FROM users")
row = cursor.fetchone()
print(row)  # (1, 'Alice')
```

#### fetchmany()

```python
Cursor.fetchmany(size: Optional[int] = None) -> List[Union[Tuple, Row]]
```

Fetch multiple rows.

**Parameters:**
- `size` (int | None): Number of rows (default: `arraysize`)

**Returns:**
- `list[tuple | Row]`: List of rows

**Examples:**
```python
cursor.execute("SELECT * FROM users")
rows = cursor.fetchmany(5)
```

#### fetchall()

```python
Cursor.fetchall() -> List[Union[Tuple, Row]]
```

Fetch all remaining rows.

**Returns:**
- `list[tuple | Row]`: All rows

**Examples:**
```python
cursor.execute("SELECT * FROM users")
all_rows = cursor.fetchall()
```

#### close()

```python
Cursor.close() -> None
```

Close the cursor.

#### setinputsizes()

```python
Cursor.setinputsizes(sizes: List[int]) -> None
```

Set input sizes (no-op, for compatibility).

#### setoutputsize()

```python
Cursor.setoutputsize(size: int, column: Optional[int] = None) -> None
```

Set output size (no-op, for compatibility).

#### Iterator Protocol

```python
for row in cursor:
    print(row)
```

Cursors are iterable:
```python
cursor.execute("SELECT * FROM users")
for row in cursor:
    print(f"User: {row[0]}, Name: {row[1]}")
```

## Row Class

### Constructor

```python
Row(cursor: Cursor, values: Tuple[Any, ...])
```

Created automatically by cursor with `row_factory = heliosdb_sqlite.Row`.

### Methods

#### Index Access

```python
row[index: int] -> Any
```

Access column by index.

**Examples:**
```python
conn.row_factory = heliosdb_sqlite.Row
cursor = conn.execute("SELECT id, name FROM users")
row = cursor.fetchone()
print(row[0])  # First column (id)
print(row[1])  # Second column (name)
```

#### Name Access

```python
row[name: str] -> Any
```

Access column by name.

**Examples:**
```python
print(row['id'])    # By name
print(row['name'])  # By name
```

#### keys()

```python
Row.keys() -> List[str]
```

Get list of column names.

**Returns:**
- `list[str]`: Column names

**Examples:**
```python
print(row.keys())  # ['id', 'name']
```

#### Length

```python
len(row) -> int
```

Number of columns.

**Examples:**
```python
print(len(row))  # 2
```

#### Iteration

```python
for value in row:
    print(value)
```

Iterate over column values.

## Exception Hierarchy

```
Exception
├── Warning
└── Error
    ├── InterfaceError
    └── DatabaseError
        ├── InternalError
        ├── OperationalError
        ├── ProgrammingError
        ├── IntegrityError
        ├── DataError
        └── NotSupportedError
```

### Error

```python
class Error(Exception)
```

Base class for all database exceptions.

### Warning

```python
class Warning(Exception)
```

Exception for important warnings.

### InterfaceError

```python
class InterfaceError(Error)
```

Errors related to database interface.

### DatabaseError

```python
class DatabaseError(Error)
```

Errors related to database operations.

### InternalError

```python
class InternalError(DatabaseError)
```

Internal database errors.

### OperationalError

```python
class OperationalError(DatabaseError)
```

Errors during database operation (connection failed, timeout, etc.).

### ProgrammingError

```python
class ProgrammingError(DatabaseError)
```

Programming errors (SQL syntax, wrong parameter count, etc.).

### IntegrityError

```python
class IntegrityError(DatabaseError)
```

Database integrity violations (constraint, unique, foreign key, etc.).

### DataError

```python
class DataError(DatabaseError)
```

Errors in processed data (type mismatch, overflow, etc.).

### NotSupportedError

```python
class NotSupportedError(DatabaseError)
```

Unsupported operations.

## Type Adapters

### Binary()

```python
Binary(data: bytes) -> bytes
```

Construct binary data for insertion.

### Date()

```python
Date(year: int, month: int, day: int) -> datetime.date
```

Construct date object.

### Time()

```python
Time(hour: int, minute: int, second: int) -> datetime.time
```

Construct time object.

### Timestamp()

```python
Timestamp(
    year: int,
    month: int,
    day: int,
    hour: int,
    minute: int,
    second: int
) -> datetime.datetime
```

Construct timestamp object.

### DateFromTicks()

```python
DateFromTicks(ticks: float) -> datetime.date
```

Construct date from UNIX timestamp.

### TimeFromTicks()

```python
TimeFromTicks(ticks: float) -> datetime.time
```

Construct time from UNIX timestamp.

### TimestampFromTicks()

```python
TimestampFromTicks(ticks: float) -> datetime.datetime
```

Construct timestamp from UNIX timestamp.

## Supported Data Types

| Python Type | SQL Type | Notes |
|-------------|----------|-------|
| `None` | `NULL` | |
| `bool` | `BOOLEAN` | Stored as TRUE/FALSE |
| `int` | `INTEGER`, `BIGINT` | |
| `float` | `REAL`, `DOUBLE` | |
| `str` | `TEXT`, `VARCHAR` | |
| `bytes` | `BLOB` | Hex encoding |
| `datetime.date` | `DATE` | ISO format |
| `datetime.time` | `TIME` | ISO format |
| `datetime.datetime` | `TIMESTAMP` | ISO format |
| `list[float]` | `VECTOR(n)` | HeliosDB extension |

## Complete Usage Example

```python
import heliosdb_sqlite as sqlite3

# Connect
conn = sqlite3.connect('myapp.db')

# Set row factory for name-based access
conn.row_factory = sqlite3.Row

# Create table
conn.execute("""
    CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        email TEXT UNIQUE,
        created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )
""")

# Insert data
conn.execute("INSERT INTO users (id, name, email) VALUES (?, ?, ?)",
             (1, 'Alice', 'alice@example.com'))

# Insert many
users = [
    (2, 'Bob', 'bob@example.com'),
    (3, 'Charlie', 'charlie@example.com')
]
conn.executemany("INSERT INTO users (id, name, email) VALUES (?, ?, ?)", users)

# Commit
conn.commit()

# Query with named parameters
cursor = conn.execute("SELECT * FROM users WHERE id = :id", {'id': 1})
row = cursor.fetchone()
print(f"User: {row['name']}, Email: {row['email']}")

# Query all
cursor = conn.execute("SELECT * FROM users ORDER BY id")
for row in cursor:
    print(f"ID: {row['id']}, Name: {row['name']}")

# Transaction with error handling
try:
    conn.execute("BEGIN")
    conn.execute("INSERT INTO users VALUES (1, 'Duplicate', 'dup@example.com')")
    conn.execute("COMMIT")
except sqlite3.IntegrityError as e:
    print(f"Error: {e}")
    conn.execute("ROLLBACK")

# Close
conn.close()
```

This API reference provides complete documentation for all `heliosdb_sqlite` functionality, ensuring developers can easily migrate from `sqlite3` and leverage HeliosDB's advanced features.
