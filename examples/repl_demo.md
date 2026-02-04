# HeliosDB Lite REPL Demo

This document demonstrates an interactive REPL session with HeliosDB Lite.

## Starting the REPL

```bash
$ heliosdb-lite repl --memory

HeliosDB Lite v0.1.0
PostgreSQL-compatible embedded database

Type \h for help, \q to quit

heliosdb>
```

## Demo Session

### 1. Show Help
```sql
heliosdb> \h

HeliosDB Lite REPL Commands
════════════════════════════════════════════════════════════

Meta Commands:
  \q, \quit, \exit  - Quit the REPL
  \h, \help, \?        - Show this help
  \d                - List all tables
  \d <table>          - Describe table schema
  \dt               - List tables with details
  \timing           - Toggle query timing
  \e, \edit          - Edit query in $EDITOR (not yet implemented)

SQL Commands:
  End SQL statements with semicolon (;)
  Multi-line statements are supported
  Press Ctrl-C to cancel current input
  Press Ctrl-D to exit

Examples:
  CREATE TABLE users (id INT, name TEXT);
  INSERT INTO users VALUES (1, 'Alice');
  SELECT * FROM users;
  \d users

heliosdb>
```

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
$ heliosdb-lite repl -d mydb

HeliosDB Lite v0.1.0
PostgreSQL-compatible embedded database

Type \h for help, \q to quit

heliosdb> CREATE TABLE persistent_data (id INT, value TEXT);
Query OK (0.002s)

heliosdb> INSERT INTO persistent_data VALUES (1, 'This data persists!');
Query OK, 1 row affected (0.001s)

heliosdb> \q
Goodbye!

# Restart REPL - data is still there!
$ heliosdb-lite repl -d mydb

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

The HeliosDB Lite REPL provides a powerful, user-friendly interface for:
- Interactive database development
- Quick data exploration
- Testing SQL queries
- Learning SQL and database concepts
- Prototyping applications

For more information, see:
- [REPL Guide](../docs/REPL_GUIDE.md) - Complete REPL documentation
- [SQL Reference](../docs/SQL_REFERENCE.md) - SQL syntax guide
- [User Guide](../README.md) - Getting started guide
