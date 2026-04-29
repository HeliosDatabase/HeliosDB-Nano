---
requested-by: Tier 1 field-bench follow-up — danimoya
requested-against: HeliosDB-Nano v3.21.1
priority: medium
status: open
date-filed: 2026-04-28
track: storage / constraints
---

# Bug: FK constraint reads pre-DELETE state from inside a transaction

## TL;DR

A two-step delete sequence inside a single transaction —
`DELETE FROM child_table WHERE …;` then
`DELETE FROM parent_table WHERE …;` — raises an FK violation on the
second statement even though the first removed every offending child
row. The FK enforcement appears to read the committed (pre-DELETE)
view of `child_table`, not the in-transaction write-set view.

## Field repro

Observed during the v3.21.1 `code_index` force-reparse ingest:

```
ERROR: Foreign key constraint
  'fk__hdb_code_symbol_refs_from_symbol___hdb_code_symbols' violated:
  cannot delete row from '_hdb_code_symbols' - referenced by
  '_hdb_code_symbol_refs'
```

In the failing run, all of the following happen within one
transaction:

```sql
DELETE FROM _hdb_code_symbol_refs WHERE file_id = 1255;  -- 181 rows deleted
DELETE FROM _hdb_code_symbols     WHERE file_id = 1255;  -- ERROR: FK violated
```

After step 1 there are no surviving refs whose `from_symbol` is in
file 1255's symbol set. But the FK check on step 2 raises as if the
first delete didn't happen.

## Hypothesis

The FK check in `EmbeddedDatabase`'s DELETE path calls something like
`storage.scan_fk_referents(parent_pk)` which probably reads the
RocksDB CF directly rather than going through `txn.scan(...)`. Result:
referents written or deleted earlier in the same txn aren't visible.

This mirrors the ON CONFLICT bug fixed in v3.20 (`txn.get(...)` for
existing-row lookup). The same fix likely applies — FK enforcement
should route through the active transaction's read view.

## Repro skeleton (unit test draft)

```rust
let db = EmbeddedDatabase::new_in_memory()?;
db.execute("CREATE TABLE parent (id INT PRIMARY KEY)")?;
db.execute("CREATE TABLE child  (id INT PRIMARY KEY,
                                  pid INT NOT NULL REFERENCES parent(id))")?;
db.execute("INSERT INTO parent (id) VALUES (1)")?;
db.execute("INSERT INTO child  (id, pid) VALUES (10, 1)")?;

db.begin()?;
db.execute("DELETE FROM child  WHERE pid = 1")?;
db.execute("DELETE FROM parent WHERE id  = 1")?;  // expected: ok; actual: FK violation
db.commit()?;
```

## Acceptance criteria

- [ ] New test
      `tests/fk_in_transaction_tests.rs::delete_parent_after_child_in_same_txn`
      reproducing the issue.
- [ ] Test passes after the fix.
- [ ] FK validator routes through the txn's read view (write-set
      first, then MVCC snapshot) instead of bypassing to RocksDB.
- [ ] No regression on existing constraint tests.

## Impact / blast radius

Affects any caller that does coordinated parent/child cascading
deletes inside an explicit transaction. Currently masked in
auto-commit DDL flows because each DELETE auto-commits. Surfaced by
v3.21.1's Tier 1.1 — wrapping `code_index`'s per-chunk writes in an
explicit transaction makes the cascading-delete sequence run inside
one txn, exposing the bug.

## Workaround until fixed

Either (a) commit between the cascading deletes (loses atomicity) or
(b) issue `SET CONSTRAINTS … DEFERRED` if the engine supports it
(currently doesn't for runtime constraint mode). For `code_index`
specifically, the v3.21.1 `force_reparse` TRUNCATE fast path avoids
the per-file cascading delete entirely, which is why the bug only
surfaces on duplicate-paths edge cases.
