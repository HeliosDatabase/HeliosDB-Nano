---
name: heliosdb-nano-transactions
description: Transactions, savepoints, isolation, and bulk-load patterns in HeliosDB-Nano. Covers BEGIN/COMMIT/ROLLBACK, nested SAVEPOINT … RELEASE/ROLLBACK TO, the embedded library's RAII Transaction handle, deadlock detection, and the fast path for inserting tens-of-thousands of rows in one transaction. Also states the two limitations that surprise people coming from PostgreSQL: SERIALIZABLE is served as snapshot isolation (write skew is not prevented), and DDL is not transactional (ROLLBACK does not undo a DROP TABLE). Use this when the user wants atomicity, multi-statement units of work, isolation levels, or fast bulk inserts.
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
| isolation (request) | SQL | `BEGIN ISOLATION LEVEL {READ COMMITTED \| REPEATABLE READ \| SERIALIZABLE}` |
| isolation (SERIALIZABLE policy) | TOML | `[storage] serializable_policy = "warn"` (or `"error"`) |
| locking / deadlocks | TOML | `[locks] deadlock_detection_enabled = true; timeout_ms = 5000` |

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

## Isolation levels — what you actually get

`BEGIN [ISOLATION LEVEL …]` is parsed on the PostgreSQL wire (the MySQL wire
reports a fixed `REPEATABLE-READ` for `@@transaction_isolation` and does not
take a per-transaction level). HeliosDB-Nano implements **snapshot isolation**;
there are only TWO distinct behaviours:

| Requested level | What runs | Anomalies still possible |
|---|---|---|
| `READ COMMITTED` (default) | fresh snapshot per statement | non-repeatable reads, phantoms |
| `REPEATABLE READ` | one snapshot per transaction + first-committer-wins write-write validation (loser gets SQLSTATE `40001`) | **write skew** |
| `SERIALIZABLE` | **identical to `REPEATABLE READ`** | **write skew** |

⚠️ **`SERIALIZABLE` is a name, not a level here.** Conflict validation
(`src/storage/conflict.rs::validate_and_record`) sees only the transaction's
WRITE set; no read set is tracked anywhere in the engine, so there is no SSI and
no dangerous-structure detection. Requesting it raises a wire `WARNING`, and
`[storage] serializable_policy = "error"` makes it a hard error instead — use
that if your correctness argument depends on serializability:

```toml
[storage]
serializable_policy = "error"   # refuse SERIALIZABLE rather than downgrade it
```

The anomaly that gets through — two transactions read the same rows, then each
writes a *different* one, so their write sets never intersect:

```sql
-- T1                                    -- T2
BEGIN ISOLATION LEVEL SERIALIZABLE;      BEGIN ISOLATION LEVEL SERIALIZABLE;
SELECT count(*) FROM doctors             SELECT count(*) FROM doctors
  WHERE on_call;            -- 2           WHERE on_call;            -- 2
UPDATE doctors SET on_call=false         UPDATE doctors SET on_call=false
  WHERE name='alice';                      WHERE name='bob';
COMMIT;                                  COMMIT;
-- PostgreSQL: one of them aborts with 40001.
-- HeliosDB-Nano: both commit. Nobody is on call.
```

To prevent write skew today, serialise the conflicting transactions yourself:
write to a common row (a counter/lock row) inside each transaction so the
write sets DO intersect and first-committer-wins fires, or use `SELECT … FOR
UPDATE` on the rows the decision depends on.

## DDL is NOT transactional

`BEGIN; DROP TABLE orders; ROLLBACK;` **destroys `orders` permanently.** Every
DDL statement writes the catalog directly instead of staging into the
transaction's write set, so:

- `CREATE TABLE` inside a transaction survives `ROLLBACK` (the table stays).
- `DROP TABLE` / `DROP INDEX` inside a transaction is applied immediately and
  `ROLLBACK` does not bring it back — the rows are gone.
- Ordinary DML in that same transaction *is* rolled back normally, so a block
  mixing both ends up half-applied.

```sql
BEGIN;
CREATE TABLE t (id INT);
INSERT INTO t VALUES (1);
ROLLBACK;
SELECT * FROM t;   -- succeeds, 0 rows: the TABLE stayed, the ROW did not
```

Run schema changes outside an explicit transaction, and take a dump
(`heliosdb-nano dump`) or a branch (`heliosdb-nano-branches`) before a
destructive migration — that branch/dump is the only undo available.
Pinned by `test_ddl_in_transaction_rollback_is_not_undone` in `src/lib.rs`.

## Pitfalls
- **An open transaction that is never committed pins memory** (write set). Always commit or rollback.
- **DDL inside a transaction is not rolled back** — see the section above. This is the single most damaging assumption to carry over from PostgreSQL.
- **Bulk inserts via the embedded library write through the transaction's write set**, not direct storage; very large bulk loads can bloat memory. The crate-internal `bulk_insert_tuples` (used by `code_index`) bypasses the write set for write-heavy ingest paths — it is `pub(crate)` and not exposed publicly. For external code, batch into commits of ~10k rows.
- **`SAVEPOINT` rollback restores the write-set snapshot** at savepoint-creation time. ART-index entries are also rolled back via the undo log. Both are bounded by the transaction's lifetime.
- **`SERIALIZABLE` does not prevent write skew** — it is served as snapshot isolation, identical to `REPEATABLE READ`. See the isolation table above; set `[storage] serializable_policy = "error"` to be told loudly instead of silently downgraded.

## See also
- `heliosdb-nano-query` — DML statements that participate in transactions.
- `heliosdb-nano-schema` — multi-op `ALTER TABLE` (atomic per statement).
- `heliosdb-nano-branches` — branches give you an alternative isolation surface for multi-step work.
- Historical FK-in-txn fix (closed in v3.22.1; see CHANGELOG v3.22.1).
