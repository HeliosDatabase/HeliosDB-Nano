---
name: heliosdb-nano-transactions
description: Transactions, savepoints, isolation, and bulk-load patterns in HeliosDB-Nano. Covers BEGIN/COMMIT/ROLLBACK, nested SAVEPOINT … RELEASE/ROLLBACK TO, the embedded library's RAII Transaction handle, deadlock detection, and the fast path for inserting tens-of-thousands of rows in one transaction. Use this when the user wants atomicity, multi-statement units of work, or fast bulk inserts.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Transactions, Savepoints & Bulk Load

## When to use
- A unit of work must be all-or-nothing.
- Nested rollback regions inside a longer txn (savepoints).
- High-volume INSERTs that the row-by-row path makes too slow.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| begin | SQL / lib | `BEGIN;` / `db.begin_transaction()` |
| commit | SQL / lib | `COMMIT;` / `tx.commit()` |
| rollback | SQL / lib | `ROLLBACK;` / `tx.rollback()` (or drop without commit) |
| savepoint | SQL | `SAVEPOINT sp1;` |
| release savepoint | SQL | `RELEASE SAVEPOINT sp1;` |
| rollback to savepoint | SQL | `ROLLBACK TO SAVEPOINT sp1;` |
| in-transaction check | lib | `db.in_transaction() -> bool` |
| isolation (configure) | TOML | `[locks] deadlock_detection_enabled = true; timeout_ms = 5000` |

## Recipes

### Recipe 1: Basic transaction (psql / any PG client)
```sql
BEGIN;
UPDATE accounts SET balance = balance - 100 WHERE id = 1;
UPDATE accounts SET balance = balance + 100 WHERE id = 2;
COMMIT;       -- atomically applied; on error/disconnect → ROLLBACK
```

### Recipe 2: Embedded (Rust) — RAII handle
```rust
use heliosdb_nano::EmbeddedDatabase;

let db = EmbeddedDatabase::new("./mydata")?;
let tx = db.begin_transaction()?;
db.execute("UPDATE accounts SET balance = balance - 100 WHERE id = 1")?;
db.execute("UPDATE accounts SET balance = balance + 100 WHERE id = 2")?;
tx.commit()?;        // explicit commit; if `tx` is dropped without commit it rolls back
```

### Recipe 3: Savepoints (nested rollback regions)
```sql
BEGIN;
INSERT INTO orders (id, total) VALUES (1, 100);

SAVEPOINT step_a;
    INSERT INTO order_items (order_id, sku) VALUES (1, 'A');
    INSERT INTO order_items (order_id, sku) VALUES (1, 'OOPS');  -- imagine this fails
ROLLBACK TO SAVEPOINT step_a;
-- order header survives, items rolled back; can retry from step_a

SAVEPOINT step_a;
    INSERT INTO order_items (order_id, sku) VALUES (1, 'A');
RELEASE SAVEPOINT step_a;
COMMIT;
```

### Recipe 4: Bulk insert via SQL (10k–100k rows)
**Inside one transaction with a single multi-row INSERT** is the fastest portable form:
```sql
BEGIN;
INSERT INTO events (ts, payload) VALUES
  (NOW(), 'a'), (NOW(), 'b'), …, (NOW(), 'zzz');   -- up to a few thousand per stmt
COMMIT;
```
For >10k rows, batch into chunks (e.g., 1000/stmt) inside one transaction. The result-cache invalidator and ART-index updates are amortised across the batch.

### Recipe 5: Bulk INSERT … SELECT
```sql
BEGIN;
INSERT INTO archive (id, body)
SELECT id, body FROM live WHERE created < NOW() - INTERVAL '7 days';
DELETE FROM live WHERE created < NOW() - INTERVAL '7 days';
COMMIT;
```
A single transaction keeps the archive + delete atomic.

### Recipe 6: Embedded library — bulk insert via batch
```rust
let tx = db.begin_transaction()?;
let stmt = "INSERT INTO events (ts, payload) VALUES ($1, $2)";
for chunk in events.chunks(1024) {
    for ev in chunk {
        db.execute_params(stmt, &[&ev.ts, &ev.payload])?;
    }
}
tx.commit()?;
```
Per-statement parameterisation hits the plan-cache after the first call; subsequent calls skip parse+plan.

### Recipe 7: Concurrent writers — handle deadlocks
Configure deadlock detection in `config.toml`:
```toml
[locks]
deadlock_detection_enabled = true
timeout_ms                 = 5000
```
On deadlock, the loser transaction receives a SQLSTATE-shaped error and the application retries with backoff. Pseudocode:
```python
import time
for attempt in range(5):
    try:
        with conn:
            conn.execute("UPDATE …")
            conn.execute("UPDATE …")
        break
    except psycopg2.errors.DeadlockDetected:
        time.sleep(0.05 * (2 ** attempt))
```

## Pitfalls
- **An open transaction that is never committed pins memory** (write set). Always commit or rollback.
- **DDL inside a transaction**: most DDL is transactional, but some forms commit implicitly (server-version dependent). When in doubt, run schema changes outside an explicit txn or check `\d` after to confirm.
- **Bulk inserts via the embedded library write through the transaction's write set**, not direct storage; very large bulk loads can bloat memory. The crate-internal `bulk_insert_tuples` (used by `code_index`) bypasses the write set for write-heavy ingest paths — it is `pub(crate)` and not exposed publicly. For external code, batch into commits of ~10k rows.
- **`SAVEPOINT` rollback restores the write-set snapshot** at savepoint-creation time. ART-index entries are also rolled back via the undo log. Both are bounded by the transaction's lifetime.
- **No explicit isolation level keyword yet** — Nano runs at a single committed-read level with MVCC snapshot per transaction. The TOML `[locks]` section is the only knob today.

## See also
- `heliosdb-nano-query` — DML statements that participate in transactions.
- `heliosdb-nano-schema` — multi-op `ALTER TABLE` (atomic per statement).
- `heliosdb-nano-branches` — branches give you an alternative isolation surface for multi-step work.
- `FEATURE_REQUEST_fk_in_txn.md` — historical FK-in-txn fix (closed in v3.22.1).
