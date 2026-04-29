---
requested-by: Tier 1 field-bench follow-up — danimoya
requested-against: HeliosDB-Nano v3.21.1
priority: high
status: open
date-filed: 2026-04-28
track: storage / correctness
---

# Bug: cross-process `INSERT … ON CONFLICT (pk) DO UPDATE` doesn't detect prior committed rows

## TL;DR

A second process attaching to an existing data directory and issuing
`INSERT INTO t (pk, …) VALUES (…) ON CONFLICT (pk) DO UPDATE SET …`
for a path that already exists in `t` (committed by a previous
process) **inserts a duplicate row** instead of taking the DO UPDATE
branch. The v3.21.0 `Catalog::rebuild_all_indexes` on engine open is
running and registers the PK ART index, but the conflict-detection
path appears not to consult the rebuilt index for cross-process state.

## Repro

```rust
// Process 1
let db = EmbeddedDatabase::new(KB_PATH)?;
db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, content TEXT)")?;
db.execute_params(
    "INSERT INTO src (path, content) VALUES ($1, $2) \
     ON CONFLICT(path) DO UPDATE SET content = excluded.content",
    &[Value::String("a.rs".into()), Value::String("v1".into())],
)?;
// Process 1 closes (Drop on db).

// Process 2 (new EmbeddedDatabase::new on the same KB_PATH)
let db = EmbeddedDatabase::new(KB_PATH)?;
db.execute_params(
    "INSERT INTO src (path, content) VALUES ($1, $2) \
     ON CONFLICT(path) DO UPDATE SET content = excluded.content",
    &[Value::String("a.rs".into()), Value::String("v2".into())],
)?;

// Expected: 1 row, content='v2'
// Actual:   2 rows, both with path='a.rs' (one v1, one v2)
```

Field-confirmed during the v3.21.1 ingest bench: the MCP plugin's
`upsert_src` (uses `ON CONFLICT(path) DO UPDATE`) inflated `src` from
663 rows to 1 326 rows over two ingests of the same corpus.

## Hypothesis

`Catalog::rebuild_all_indexes` (added in v3.21.0) walks every user
table on `EmbeddedDatabase::new` and re-populates the in-memory ART
indexes from on-disk rows. That path is correct — verified by
`tests/cross_process_index_rebuild_tests.rs`'s `pk_uniqueness_enforced_after_reopen`
which DOES catch a duplicate plain-INSERT.

But the ON CONFLICT path in `lib.rs` calls
`art_indexes().check_unique_constraints(...)` and only takes the
DO UPDATE branch if that returns `Err(DuplicateKey)`. If the rebuild
skips a step (e.g. doesn't actually flag duplicates against the
DataKey-encoded form ON CONFLICT uses) the conflict goes undetected.

The plain-INSERT test passing while ON CONFLICT fails suggests the
rebuild populates the PK index correctly but `check_unique_constraints`
either uses a different code path / encoding or has a stale-cache
bug specific to ON CONFLICT.

## Acceptance criteria

- [ ] A unit test in `tests/cross_process_index_rebuild_tests.rs`
      reproducing the duplicate-row case via `INSERT … ON CONFLICT`.
- [ ] The test passes.
- [ ] No regression on `cross_process_index_rebuild_tests.rs`
      existing 6 tests.

## Workaround until fixed

For batch upsert clients (MCP plugin, anyone walking + upserting a
fresh source tree on each run): clear the destination table before
the upsert loop, e.g. `DELETE FROM src;` inside the same transaction
as the upserts. Trades a full rewrite per ingest for correctness.
