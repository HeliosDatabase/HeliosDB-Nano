# HeliosDB Lite Feature Test Tracker
**Date:** 2026-02-03
**Version:** 3.5.8 (with fixes applied)
**Tester:** Claude Code Automated Testing

---

## Executive Summary

| Category | Status | Pass | Fail | Critical |
|----------|--------|------|------|----------|
| Core SQL | ✅ Mostly Working | 14 | 2 | 0 |
| REPL Meta-Commands | ✅ Mostly Working | 18 | 3 | 0 |
| Branching | ✅ Working | 4 | 1 | 0 |
| Time-Travel | ⚠️ Architectural | 1 | 2 | 1 |
| Vector Search (SQL) | ✅ Working | 3 | 1 | 0 |
| Multi-Tenancy | ✅ Working | 6 | 0 | 0 |
| Triggers | ❌ Not Implemented | 0 | 2 | 1 |
| Constraints | ✅ Working | 3 | 0 | 0 |
| Transactions | ✅ Working | 4 | 0 | 0 |
| Server Mode | ⚠️ Issues | 3 | 2 | 1 |
| Python SDK (Embedded) | ❌ Broken | 1 | 3 | 1 |
| Python SDK (Daemon) | ⚠️ Issues | 1 | 2 | 1 |

**Overall Status:** ⚠️ Several critical issues fixed. Remaining issues: Time-travel (architectural), Triggers (not implemented), Python SDK.

---

## Fixes Applied (2026-02-03)

### ✅ Fix 1: Transaction COMMIT/ROLLBACK
- **Issue:** COMMIT/ROLLBACK failed with "Operator not yet implemented"
- **Root cause:** `is_transaction_control()` didn't strip trailing semicolons
- **Fix:** Added `.trim_end_matches(';')` to transaction control detection
- **Status:** WORKING - Both COMMIT and ROLLBACK now work correctly

### ✅ Fix 2: NOT NULL Constraint Enforcement
- **Issue:** NULL values could be inserted into NOT NULL columns
- **Root cause:** Constraint not validated during INSERT
- **Fix:** Added validation check in INSERT execution path
- **Status:** WORKING - NULL inserts now rejected with proper error message

### ✅ Fix 3: CHECK Constraint Enforcement
- **Issue:** CHECK constraints ignored (e.g., `price > 0` accepted negative values)
- **Root cause:** Column-level CHECK expressions stored as JSON but parsed as SQL
- **Fix:**
  1. Added extraction of column-level CHECK constraints in planner
  2. Modified `evaluate_check_constraint` to deserialize JSON LogicalExpr
- **Status:** WORKING - Invalid values now rejected with CHECK violation error

### ✅ Fix 4: RLS Multi-Tenancy Isolation
- **Issue:** Data from tenant A visible to tenant B
- **Root cause:** FilteredScan operator not handled in RLS plan transformation
- **Fix:** Added FilteredScan case in `apply_rls_to_plan_recursive`
- **Status:** WORKING - RLS policies now properly filter data per tenant
- **Note:** Policies must be explicitly created: `\tenant rls create <table> <policy> <expr> ALL`

---

## Remaining Critical Issues

### 1. Time-Travel Queries (Architectural Limitation)
- `AS OF TRANSACTION <n>` fails: "Transaction not found or has been garbage collected"
- `AS OF TIMESTAMP '<ts>'` fails: "No snapshot found for timestamp"
- **Root cause:** In-memory mode doesn't automatically create snapshots for each transaction
- **Impact:** Time-travel requires explicit snapshot management
- **Status:** Requires architectural changes to storage engine

### 2. Triggers Not Implemented
- CREATE TRIGGER fails with "Operator not yet implemented: CreateTrigger"
- **Status:** Requires trigger operator implementation

### 3. Python SDK Embedded Mode Architecture
- Each SQL call spawns a new REPL process with `--memory`
- All data lost between calls
- **Status:** Requires architectural change to maintain persistent process

### 4. Python SDK Embedded Mode Fundamentally Broken (CRITICAL)
- Each SQL call spawns a **new REPL process** with `--memory`
- **All data lost between calls** - INSERT in one call, SELECT in next returns empty
- `cursor.fetchall()` returns `[]` even after successful INSERT
- `cursor.description` returns `None`

---

## Detailed Test Results

### 1. REPL Meta-Commands

| Command | Status | Notes |
|---------|--------|-------|
| `\h` | ✅ Pass | Help displays correctly |
| `\d` | ✅ Pass | Lists tables |
| `\d <table>` | ✅ Pass | Shows table schema |
| `\dt` | ✅ Pass | Lists tables with details |
| `\dS` | ✅ Pass | Lists system views |
| `\timing` | ✅ Pass | Toggles query timing |
| `\branches` | ✅ Pass | Lists branches |
| `\use <branch>` | ✅ Pass | Switches branches |
| `\snapshots` | ✅ Pass | Lists snapshots (empty) |
| `\dmv` | ✅ Pass | Lists materialized views |
| `\config` | ✅ Pass | Shows configuration |
| `\stats` | ✅ Pass | Shows database stats |
| `\compression` | ✅ Pass | Shows compression stats |
| `\server status` | ✅ Pass | Shows server status |
| `\tenant list/create/use` | ✅ Pass | Tenant management |
| `\vectors` | ⚠️ Partial | Lists stores but creation fails |
| `\vector create` | ❌ Fail | "Vector store operations not yet implemented" |
| `\show lsn` | ✅ Pass | Shows LSN toggle |
| `\q` | ✅ Pass | Quits REPL |

### 2. Core SQL Features

| Feature | Status | Notes |
|---------|--------|-------|
| CREATE TABLE | ✅ Pass | Works correctly |
| DROP TABLE | ✅ Pass | Works correctly |
| INSERT | ✅ Pass | Works correctly |
| UPDATE | ✅ Pass | Basic UPDATE works |
| DELETE | ✅ Pass | Works correctly |
| SELECT * | ✅ Pass | Works correctly |
| SELECT with WHERE | ✅ Pass | Works correctly |
| SELECT with ORDER BY | ⚠️ Issues | Column aliases show as col_0, col_1 |
| SELECT with LIMIT/OFFSET | ✅ Pass | Works correctly |
| JOIN | ✅ Pass | Works correctly, but aliases show col_0, etc. |
| GROUP BY | ✅ Pass | Works, aliases show group_0, agg_0 |
| Aggregate functions | ✅ Pass | SUM, AVG, COUNT, MAX, MIN work |
| CREATE INDEX | ✅ Pass | Works correctly |
| Subqueries (IN) | ❌ Fail | "Expression not yet supported: InSubquery" |
| SQL comments (--) | ❌ Fail | "No SQL statement found" |
| Type coercion | ❌ Fail | "Cannot subtract Float4 and Int4" |
| BEGIN | ✅ Pass | Starts transaction |
| COMMIT | ✅ Pass | **FIXED** - Commits transaction, data now visible |
| ROLLBACK | ✅ Pass | **FIXED** - Rollback works, data discarded |

### 3. Branching

| Feature | Status | Notes |
|---------|--------|-------|
| CREATE BRANCH (no AS OF) | ❌ Fail | Requires AS OF clause |
| CREATE BRANCH AS OF NOW | ✅ Pass | Works correctly |
| \use <branch> | ✅ Pass | Switches branches |
| Branch isolation | ✅ Pass | Data isolated between branches |
| MERGE BRANCH | Not tested | |
| DROP BRANCH | Not tested | |

### 4. Time-Travel

| Feature | Status | Notes |
|---------|--------|-------|
| AS OF TRANSACTION | ❌ Fail | "Transaction X not found or garbage collected" |
| AS OF TIMESTAMP | ❌ Fail | "No snapshot found for timestamp" |
| AS OF NOW | ⚠️ Works | But returns data in different order |

### 5. Vector Search

| Feature | Status | Notes |
|---------|--------|-------|
| CREATE TABLE with VECTOR | ✅ Pass | Works correctly |
| INSERT vector data | ✅ Pass | Works correctly |
| Distance operator (<->) | ✅ Pass | Works for similarity search |
| \vector create (REPL) | ❌ Fail | Not implemented |

### 6. Materialized Views

| Feature | Status | Notes |
|---------|--------|-------|
| CREATE MATERIALIZED VIEW | ✅ Pass | Works correctly |
| SELECT from MV | ✅ Pass | Works, aliases show group_0, agg_0 |
| REFRESH MATERIALIZED VIEW | ✅ Pass | Works correctly |
| \dmv | ✅ Pass | Lists MVs |

### 7. Triggers

| Feature | Status | Notes |
|---------|--------|-------|
| CREATE TRIGGER (EXECUTE FUNCTION) | ❌ Fail | "Operator not yet implemented: CreateTrigger" |
| CREATE TRIGGER (BEGIN/END) | ❌ Fail | Parse error |

### 8. Constraints

| Feature | Status | Notes |
|---------|--------|-------|
| PRIMARY KEY | ✅ Pass | Enforced |
| NOT NULL | ✅ Pass | **FIXED** - NULL insertion rejected |
| CHECK | ✅ Pass | **FIXED** - Invalid values rejected |
| UNIQUE | Not tested | |
| FOREIGN KEY | Not tested | |

### 9. Server Mode (PostgreSQL Protocol)

| Feature | Status | Notes |
|---------|--------|-------|
| Start daemon | ✅ Pass | Works correctly |
| Connect with psql | ✅ Pass | Works correctly |
| SELECT | ✅ Pass | Works correctly |
| INSERT | ⚠️ Issues | Need explicit transaction + COMMIT |
| Transaction handling | ⚠️ Issues | "Transaction already active" errors |

### 10. Python SDK

#### Embedded Mode (heliosdb_sqlite)
| Feature | Status | Notes |
|---------|--------|-------|
| Import | ✅ Pass | Imports correctly |
| connect(':memory:') | ⚠️ Issues | Connects but data not persisted |
| cursor.execute() | ⚠️ Issues | Executes but spawns new process each time |
| cursor.fetchall() | ❌ Fail | Returns empty list |
| cursor.description | ❌ Fail | Returns None |
| cursor.rowcount | ❌ Fail | Returns -1 |

#### Daemon Mode
| Feature | Status | Notes |
|---------|--------|-------|
| Connect via psycopg2 | ✅ Pass | Works correctly |
| SELECT | ⚠️ Issues | Returns empty rows without COMMIT |
| heliosdb_sqlite daemon | ❌ Fail | "Transaction already active" error |

---

## Column Alias Issue (Cosmetic)

Throughout testing, column aliases are not preserved:
- `SELECT name, age FROM users ORDER BY age` → columns shown as `col_0`, `col_1`
- `SELECT customer, SUM(amount) as total` → columns shown as `group_0`, `agg_0`
- `SELECT MAX(price), MIN(price)` → columns shown as `agg_0`, `agg_1`

---

## Recommendations

### Priority 1 (Critical - Block Production)
1. **Fix COMMIT/ROLLBACK** - Implement transaction commit and rollback operators
2. **Fix Time-Travel** - Ensure AS OF TRANSACTION and TIMESTAMP work
3. **Enforce Constraints** - Implement NOT NULL and CHECK constraint enforcement
4. **Fix Tenant Isolation** - RLS policies must actually filter data
5. **Fix Python SDK** - Maintain persistent REPL process or use file-based storage

### Priority 2 (High - Affects Usability)
1. **Implement Subqueries** - IN subquery support
2. **Fix Type Coercion** - Auto-convert between numeric types
3. **Support SQL Comments** - Parse and ignore `--` comments
4. **Implement Triggers** - CreateTrigger operator

### Priority 3 (Medium - Polish)
1. **Preserve Column Aliases** - Show actual column names/aliases in output
2. **Vector Store REPL Commands** - Implement \vector create, stats

---

## Test Environment

- **Binary:** `./target/release/heliosdb-lite`
- **Rust Version:** 1.92.0
- **Build:** Release profile
- **Test Mode:** In-memory (`--memory`)
- **Python:** 3.9
- **psql:** PostgreSQL client

---

## Appendix: Reproduction Commands

### Transaction Failure
```sql
CREATE TABLE t (id INT);
INSERT INTO t VALUES (1);
BEGIN;
UPDATE t SET id = 2;
COMMIT;  -- ERROR: Operator not yet implemented: Commit
```

### Time-Travel Failure
```sql
CREATE TABLE logs (id INT, msg TEXT);
INSERT INTO logs VALUES (1, 'First');
SELECT * FROM logs AS OF TRANSACTION 6;
-- ERROR: Transaction 6 not found or has been garbage collected
```

### Constraint Not Enforced
```sql
CREATE TABLE products (id INT PRIMARY KEY, name TEXT NOT NULL, price REAL CHECK (price > 0));
INSERT INTO products VALUES (1, NULL, -10);  -- Should fail, but succeeds
```

### Python SDK Failure
```python
import heliosdb_sqlite as sqlite3
conn = sqlite3.connect(':memory:')
cursor = conn.cursor()
cursor.execute('CREATE TABLE t (id INT)')
cursor.execute('INSERT INTO t VALUES (1)')
cursor.execute('SELECT * FROM t')
print(cursor.fetchall())  # Returns [] instead of [(1,)]
```
