# Task 205 — CloudV2 admin_db persistence bug

## Reference

`/home/app/Helios/CloudV2/docs/PERSISTENCE-BUG-INVESTIGATION.md`

## Goal of this task

Step 8.4 of the investigation doc: build a minimal Nano-only repro
to determine whether the bug is in the engine or in the
deadpool/admin_db wrapper.

## What was built

`tests/persistence_repro.rs` — a single `#[ignore]`d integration
test that:

1. Boots an `EmbeddedDatabase` + `PgServer` in-process on a
   random loopback port.
2. Opens **one** `tokio_postgres` simple-protocol connection
   (mirroring deadpool's `max_size: 1`).
3. Issues `batch_execute(INSERT …)` followed by
   `batch_execute("COMMIT")` — exactly CloudV2's
   `admin_db::simple_execute` shape.
4. Reads the row back with `simple_query("SELECT … WHERE id =
   '<uuid>'")` and `simple_query("SELECT id FROM databases")`.
5. Asserts both reads see the row.

## Result

```
$ cargo test --test persistence_repro -- --ignored --nocapture
running 1 test
thread 'insert_then_select_visible_on_same_connection' panicked at
  tests/persistence_repro.rs:92:5:
  INSERT-then-SELECT lost the row on the same connection —
  the CloudV2 persistence bug reproduces against Nano alone.
```

**The bug reproduces against Nano alone.** No deadpool, no
admin_db wrapper, no Cloud REST. This confirms Theories A and / or
C from the CloudV2 investigation doc:

- **Theory A** — `batch_execute("COMMIT")` doesn't reliably close
  Nano's implicit transaction. The implicit txn carrying the
  INSERT stays open; on the next batch_execute it's already too
  late, and on connection recycle the INSERT is rolled back.
- **Theory C** — Nano 3.14.9+ regression in simple-protocol
  COMMIT semantics (the FR codebase has had heavy DML churn
  since the 3.6 era when the original "DML needs explicit
  COMMIT" pattern was authored).

## Eliminated theories

- **Theory B** (deadpool `Fast` recycling) — out, since the repro
  uses no deadpool at all.
- **Theory D** (tokio_postgres simple-query streaming bug) —
  unlikely; same client works against vanilla Postgres in the
  Cloud test fleet.

## Next-step plan (engine-side)

1. Run the repro with `RUST_LOG=heliosdb_nano::protocol::postgres=trace`
   and capture the wire-level `CommandComplete` /
   `ReadyForQuery` payloads after the INSERT and the COMMIT.
2. Inspect Nano's simple-protocol handler for the
   `Statement::Commit` arm — look for whether the implicit txn
   actually flushes WAL and updates row visibility.
3. Try the workaround the investigation doc suggests: combine
   INSERT + COMMIT into a single `batch_execute` payload and see
   whether the bug disappears.
4. If the combined-payload workaround works, file it as the
   short-term mitigation and ship a proper fix that makes
   `COMMIT;` idempotent at the protocol layer.

## Disposition for v3.19.0

The repro lands in this release as `#[ignore]`d; the actual fix
is engine-side and warrants its own focused PR. The
investigation doc + this reproduction test are the deliverables
that unblock the engine-side debugging.

Mark CloudV2's `cloud-v2.heliosdb.com` as **staging only** until
the engine fix lands, per the investigation doc's operational
guidance.
