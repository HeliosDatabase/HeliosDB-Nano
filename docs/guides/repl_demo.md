# HeliosDB Nano REPL Demo

This document demonstrates an interactive REPL session with HeliosDB Nano.

## Starting the REPL

```bash
$ heliosdb-nano repl --memory

HeliosDB Nano
PostgreSQL-compatible embedded database

Type \h for help, \q to quit

heliosdb>
```

## Demo Session

### 1. Show Help
```sql
heliosdb> \h

╔══════════════════════════════════════════════════════════════════════════════╗
║              HeliosDB Nano - Interactive SQL REPL Help                      ║
╚══════════════════════════════════════════════════════════════════════════════╝

Quick Start:
  \h  Show this help
  \h <category>  Show category help (e.g., \h vectors)
  \q  Exit REPL

Help Categories:
  basics  Fundamental REPL operations
  schema  Table and view exploration
  branching  Database branching workflows
  time-travel  Query historical data
  vectors  Vector search and indexing
  documents  Document storage and RAG
  agents  AI agent sessions
  ai  AI inference and generation
  tenants  Multi-tenant isolation
  settings  Configuration and tuning
  examples  Practical SQL examples
  sql  SQL syntax reference

heliosdb> \h basics
```

The backslash commands of every category come from one table
(`src/repl/registry.rs`), so the help lists exactly the commands the shell accepts and nothing
else. The basics:

```
  \q, \quit, \exit              Quit REPL
  \h, \help, \?                 Show help menu
  \h <category>                 Show category-specific help
  \timing [on|off]              Show or set query execution timing (bare \timing toggles)
  \x [on|off|auto]              Expanded output: one block per row
  \pset <option> [value]        Set a display option: format, null, border, maxrows, maxwidth, ...
  \d, \d <table>, \dt           List tables, describe a table
  \conninfo                     Show what this shell is connected to
```

A statement ends at a semicolon that is outside a quote, a `$$` body and a comment; an open
construct continues on the next line (see "Multi-line bodies and schema files"). Ctrl-C cancels
the open statement and Ctrl-D exits.

### Multi-line bodies and schema files

The shell cuts its input at the semicolons that are outside a quote, a
dollar-quoted body and a comment, by the same rules the PostgreSQL wire uses, so
a function body can be typed over several lines and a schema file can be piped in:

```sql
heliosdb> CREATE FUNCTION inc(x INT) RETURNS INT AS $$
       $> BEGIN
       $>   RETURN $1 + 1;
       $> END;
       $> $$ LANGUAGE plpgsql;
Query OK
heliosdb> SELECT inc(1);
```

```bash
heliosdb-nano repl --data-dir ./mydata < schema.sql
```

A statement that is open when the input ends (a string or comment never closed)
is reported on stderr and not run; a final statement that only lacks its
semicolon is run.

### Scripting

`repl` (alias `sql`) runs SQL without a prompt when given `-c` or `-f`: no banner, no history file,
results on stdout in the chosen `--format`, errors on stderr.

```bash
heliosdb-nano sql --memory -c "SELECT 1 AS a" --format json        # [{"a":1}]
heliosdb-nano sql --data-dir ./db -f schema.sql                     # a file; `-f -` is standard input
heliosdb-nano sql --data-dir ./db -1 -f load.sql                    # one transaction (data changes only)
heliosdb-nano sql --data-dir ./db --config repl.toml -c "SELECT 1"  # shell settings from a file
```

| Flag | Meaning |
|------|---------|
| `-c SQL` / `-f FILE` | run this SQL / this file (repeatable; commands first, then files) |
| `--format F` | `table`, `csv`, `tsv`, `json` (one array per statement), `jsonl`, `vertical`, `markdown` |
| `-t` | no header row and no row count |
| `--null TEXT` | how NULL is written (empty for csv/tsv/markdown, `NULL` for table/vertical, `null` in JSON) |
| `-1` | one transaction: `COMMIT` at the end, `ROLLBACK` at the first failure. DDL is not transactional in Nano, and `CALL` or a user-defined function inside a transaction is refused, so the rollback is complete for data changes only |
| `--on-error-stop=false` | run on after a failure (still exits 3 at the end) |
| `--config FILE` | `output_format`, `null_display`, `show_timing`, ... for the shell; `\config reload` re-reads it |

Exit codes: 0 every statement ran, 1 usage or a fatal error (an unreadable file), 2 the database could
not be opened, 3 a statement failed. With `--format json` a failed statement is one JSON object
(`error`, `statement`, `sql`) on stderr.

### Reading wide and long results

At a terminal results are drawn for reading: one rule under the header and none between rows,
numbers right-aligned, a cell wider than `max_column_width` cut with `…`, columns that do not fit the
window left out with a marker and a footer saying how many, and a result taller than the screen sent
through `$PAGER` (default `less -FRSX`). Through a pipe the output is the plain table, unchanged.

| Command | Effect |
|---------|--------|
| `\x [on\|off\|auto]` | one block per row (`-[ RECORD n ]`); `auto` does it only when the table is wider than the window |
| `\pset format F` | `table`, `csv`, `tsv`, `json`, `jsonl`, `vertical`, `markdown` |
| `\pset null TEXT` | how NULL is shown (`\pset null '(null)'`) |
| `\pset border 0\|1\|2` | no lines, a rule under the header, a full box |
| `\pset maxrows N` | show the first and last N/2 rows with a footer; `0` (the default) shows all |
| `\pset maxwidth N` | cut cells at N columns; `0` shows them whole |
| `\pset pager on\|off` | use `$PAGER` for a result taller than the screen |
| `\t [on\|off]` | no header and no row count |
| `\a` | toggle aligned and unaligned (`\|`-separated) output |

A setting you change applies to a pipe as well; nothing you did not ask for does.

### What a statement prints

Whether a statement shows rows or an affected count is decided by the engine, not
by its first word: `(SELECT 1) UNION (SELECT 2)`, `EXECUTE` of a prepared query,
`SHOW ...`, `INSERT ... RETURNING` (with the names the clause gives the columns)
and the `SHOW CLUSTER STATUS` family print rows; `INSERT INTO t VALUES ('x returning y')`
prints `Query OK, 1 row(s) affected`.

### 2. Create Tables
```sql
heliosdb> CREATE TABLE users (
       ->   id INT PRIMARY KEY,
       ->   name TEXT NOT NULL,
       ->   email TEXT,
       ->   created_at TIMESTAMP
       -> );
Query OK (0.002s)

heliosdb> CREATE TABLE orders (
       ->   id INT PRIMARY KEY,
       ->   user_id INT,
       ->   amount FLOAT8,
       ->   status TEXT
       -> );
Query OK (0.001s)

heliosdb> \d

Tables:
  orders
  users
```

### 3. Describe Table Schema
```sql
heliosdb> \d users

Table: users
──────────────────────────────────────────────────
Column               Type            Nullable   Primary Key
──────────────────────────────────────────────────
id                   Int4            NO         YES
name                 Text            NO
email                Text            YES
created_at           Timestamp       YES
```

### 4. Insert Data
```sql
heliosdb> INSERT INTO users (id, name, email)
       -> VALUES (1, 'Alice Smith', 'alice@example.com'),
       ->        (2, 'Bob Johnson', 'bob@example.com'),
       ->        (3, 'Charlie Brown', 'charlie@example.com');
Query OK, 3 rows affected (0.001s)

heliosdb> INSERT INTO orders (id, user_id, amount, status)
       -> VALUES (101, 1, 150.00, 'completed'),
       ->        (102, 1, 75.50, 'pending'),
       ->        (103, 2, 200.00, 'completed'),
       ->        (104, 3, 99.99, 'shipped');
Query OK, 4 rows affected (0.001s)
```

### 5. Query Data
```sql
heliosdb> SELECT * FROM users;
┌────┬────────────────┬────────────────────────┬────────────┐
│ id │ name           │ email                  │ created_at │
├────┼────────────────┼────────────────────────┼────────────┤
│ 1  │ Alice Smith    │ alice@example.com      │ NULL       │
│ 2  │ Bob Johnson    │ bob@example.com        │ NULL       │
│ 3  │ Charlie Brown  │ charlie@example.com    │ NULL       │
└────┴────────────────┴────────────────────────┴────────────┘
(3 rows) (0.001s)

heliosdb> SELECT * FROM orders WHERE status = 'completed';
┌─────┬─────────┬────────┬───────────┐
│ id  │ user_id │ amount │ status    │
├─────┼─────────┼────────┼───────────┤
│ 101 │ 1       │ 150.00 │ completed │
│ 103 │ 2       │ 200.00 │ completed │
└─────┴─────────┴────────┴───────────┘
(2 rows) (0.002s)
```

### 6. Aggregate Queries
```sql
heliosdb> SELECT
       ->   status,
       ->   COUNT(*) as order_count,
       ->   SUM(amount) as total_amount,
       ->   AVG(amount) as avg_amount
       -> FROM orders
       -> GROUP BY status;
┌───────────┬─────────────┬──────────────┬────────────┐
│ status    │ order_count │ total_amount │ avg_amount │
├───────────┼─────────────┼──────────────┼────────────┤
│ completed │ 2           │ 350.00       │ 175.00     │
│ pending   │ 1           │ 75.50        │ 75.50      │
│ shipped   │ 1           │ 99.99        │ 99.99      │
└───────────┴─────────────┴──────────────┴────────────┘
(3 rows) (0.003s)
```

### 7. Update Data
```sql
heliosdb> UPDATE orders SET status = 'completed' WHERE id = 102;
Query OK, 1 row affected (0.001s)

heliosdb> SELECT * FROM orders WHERE user_id = 1;
┌─────┬─────────┬────────┬───────────┐
│ id  │ user_id │ amount │ status    │
├─────┼─────────┼────────┼───────────┤
│ 101 │ 1       │ 150.00 │ completed │
│ 102 │ 1       │ 75.50  │ completed │
└─────┴─────────┴────────┴───────────┘
(2 rows) (0.001s)
```

### 8. Delete Data
```sql
heliosdb> DELETE FROM orders WHERE status = 'shipped';
Query OK, 1 row affected (0.001s)

heliosdb> SELECT COUNT(*) as remaining_orders FROM orders;
┌──────────────────┐
│ remaining_orders │
├──────────────────┤
│ 3                │
└──────────────────┘
(1 row) (0.001s)
```

### 9. Join Query
```sql
heliosdb> SELECT u.name, COUNT(o.id) as order_count, SUM(o.amount) as total_spent
       -> FROM users u
       -> LEFT JOIN orders o ON u.id = o.user_id
       -> GROUP BY u.name;
┌────────────────┬─────────────┬─────────────┐
│ name           │ order_count │ total_spent │
├────────────────┼─────────────┼─────────────┤
│ Alice Smith    │ 2           │ 225.50      │
│ Bob Johnson    │ 1           │ 200.00      │
│ Charlie Brown  │ 0           │ NULL        │
└────────────────┴─────────────┴─────────────┘
(3 rows) (0.004s)
```

### 10. Toggle Timing
```sql
heliosdb> \timing
Timing is off

heliosdb> SELECT * FROM users LIMIT 1;
┌────┬──────────────┬───────────────────┬────────────┐
│ id │ name         │ email             │ created_at │
├────┼──────────────┼───────────────────┼────────────┤
│ 1  │ Alice Smith  │ alice@example.com │ NULL       │
└────┴──────────────┴───────────────────┴────────────┘
(1 row)

heliosdb> \timing
Timing is on
```

### 11. List All Tables
```sql
heliosdb> \dt

Tables:
────────────────────────────────────────────────────────────
Name                           Columns
────────────────────────────────────────────────────────────
orders                         4
users                          4
```

### 12. Exit
```sql
heliosdb> \q
Goodbye!
```

## Advanced Features

### Auto-Completion Demo
```sql
heliosdb> SEL<TAB>
→ SELECT

heliosdb> SELECT * FROM us<TAB>
→ SELECT * FROM users

heliosdb> SELECT na<TAB>
→ SELECT name
```

### Multi-Line Cancel
```sql
heliosdb> SELECT * FROM users
       -> WHERE id = 1
       -> AND name LIKE 'A%'
       -> ^C
heliosdb>
```

### History Navigation
Use ↑ and ↓ to navigate previous commands
Use Ctrl-R for reverse search

### Error Handling
```sql
heliosdb> SELECT * FROM nonexistent_table;
ERROR: Table 'nonexistent_table' does not exist

heliosdb> INSERT INTO users (invalid_column) VALUES (1);
ERROR: Column 'invalid_column' does not exist
```

## Running with Persistent Storage

```bash
# Create a database directory
$ mkdir mydb

# Start REPL with persistent storage
$ heliosdb-nano repl -d mydb

HeliosDB Nano
PostgreSQL-compatible embedded database

Type \h for help, \q to quit

heliosdb> CREATE TABLE persistent_data (id INT, value TEXT);
Query OK (0.002s)

heliosdb> INSERT INTO persistent_data VALUES (1, 'This data persists!');
Query OK, 1 row affected (0.001s)

heliosdb> \q
Goodbye!

# Restart REPL - data is still there!
$ heliosdb-nano repl -d mydb

heliosdb> SELECT * FROM persistent_data;
┌────┬──────────────────────┐
│ id │ value                │
├────┼──────────────────────┤
│ 1  │ This data persists!  │
└────┴──────────────────────┘
(1 row) (0.001s)
```

## Tips for Effective REPL Usage

1. **Use \d commands frequently** to understand your schema
2. **Format multi-line queries** for better readability
3. **Keep timing on** during development to identify slow queries
4. **Leverage history** with ↑/↓ to avoid retyping
5. **Use auto-completion** (Tab) to speed up typing
6. **Test in --memory mode** for quick experiments
7. **Use persistent mode** for development databases

## Performance Example

```sql
heliosdb> \timing
Timing is on

heliosdb> CREATE TABLE large_table (id INT, data TEXT);
Query OK (0.002s)

heliosdb> -- Batch insert (simulated)
heliosdb> INSERT INTO large_table
       -> SELECT generate_series(1, 1000), 'test data';
Query OK, 1000 rows affected (0.125s)

heliosdb> SELECT COUNT(*) FROM large_table;
┌───────┐
│ count │
├───────┤
│ 1000  │
└───────┘
(1 row) (0.003s)
```

## Conclusion

The HeliosDB Nano REPL provides a powerful, user-friendly interface for:
- Interactive database development
- Quick data exploration
- Testing SQL queries
- Learning SQL and database concepts
- Prototyping applications

For more information, see:
- [REPL Guide](../docs/REPL_GUIDE.md) - Complete REPL documentation
- [SQL Reference](../docs/SQL_REFERENCE.md) - SQL syntax guide
- [User Guide](../README.md) - Getting started guide
