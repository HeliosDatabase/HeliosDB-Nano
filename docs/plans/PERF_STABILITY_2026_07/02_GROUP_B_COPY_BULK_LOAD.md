# Group B — COPY bulk-load fast path

**Target:** T2 — COPY 100k rows: 2.3 s (23 µs/row) → ≤0.5 s (≤5 µs/row), single-commit atomicity (PG semantics).
**Branch:** `perf/copy-bulk-load` · **Risk:** medium (new bulk path; guarded by fallback gates) · **Effort:** M

## Diagnosis (analysis agent, HEAD @ 68e814a)

The wire COPY path never touches the shipped fast-batch machinery. Today, `handle_copy`
(`src/protocol/postgres/handler.rs:1371`) buffers all CopyData, decodes to
`Vec<Vec<Option<String>>>`, then per 500-row chunk **renders a ~25KB multi-row INSERT SQL
string** (`copy.rs:205`), calls session-unaware `database.execute(&sql)` → full sqlparser
parse (`lib.rs:2281`, always a cache miss, plus AST deep-clone *into* the parse cache =
pure pollution), planner, and the **generic per-row Insert plan arm** (`lib.rs:2476-3190`):
~6 materializations per field, 2 String-keyed HashMaps per row, per-row constraints clone,
per-row **dedicated RocksDB write for the logical-WAL entry** (`engine.rs:3132` →
`wal.rs:517`), per-row ART global-RwLock round-trips, then a commit per chunk with
**3 MVCC keys/row** (time-travel default). 200 commits per 100k-row COPY; zero fsyncs at
default `durable_commit=false` — the 23 µs/row is CPU + RocksDB write-path work.

Cost split ≈ parse/plan/AST 25-35% · per-row plan-arm machinery 35-45% · per-row WAL
RocksDB write 15-25% · commit+MVCC triple-write 10-15%.

**Correctness bugs found in passing:**
- COPY is **not atomic**: crash/constraint-failure at chunk k leaves chunks 1..k-1 committed (PG is all-or-nothing).
- `BEGIN; COPY; ROLLBACK` over the wire **leaks rows** — `handle_copy` bypasses the session transaction.

## Changes

### B1 (primary): typed bulk path `handle_copy` → fast-batch machinery
New `pub(crate) EmbeddedDatabase::copy_bulk_insert(table, columns, rows: Vec<Vec<Option<String>>>) -> Result<u64>`
(next to `try_fast_insert_many_params`, `lib.rs:~6177`), called from `handler.rs:1435-1444`
replacing the chunk loop:

1. **Fallback gates** (return None → today's SQL path, semantics preserved): triggers on
   table; RLS/tenant active; FK or CHECK constraints present; active branch; open
   session/global txn (until B4); Dictionary/CAS column storage;
   `fast_dml_requires_logical_wal()` (HA primary / `logical_wal_per_statement`).
2. **Typed decode, no SQL text**: resolve schema once; `Option<String>` → `Value::Null` /
   `Value::String` (move) / `fast_cast_value` (`lib.rs:6648`) for non-text targets;
   NOT-NULL via existing helpers.
3. **One batch**: `prepare_fast_insert_batch` (`lib.rs:8345`) →
   `validate_fast_insert_batch` (`lib.rs:8195`, ART probes + composite-safe intra-batch
   dedup) → one `insert_prepared_tuples_fast_batch` (`engine.rs:10282`; single WriteBatch,
   one `db.write`, one snapshot ts, no per-row logical WAL) → `durable_autocommit_barrier()`
   + `increment_lsn()` → one `invalidate_result_cache()`. One `suspend_smfi_for_bulk_load`
   guard for the whole COPY. Report the true row count in the `COPY n` tag (`handler.rs:1445`).
4. **Atomicity**: whole COPY = one batch at CopyDone. Validation runs before any write ⇒
   constraint failure rejects the whole COPY (**behavior change → changelog**). Cap ~1M
   rows / ~256MB estimated batch; above it, one autocommit txn + `put_insert_fast` per row
   with a single commit (still all-or-nothing).

### B2: multi-row literal INSERT fast path on the implicit-txn route
`lib.rs:2151` gates `try_fast_insert_literal_in_transaction` behind `skip_fast_paths`
(explicit txns only). Extend the autocommit route so multi-row literal VALUES uses the
value-group parser (`lib.rs:8134-8155`) + `insert_validated_tuples_in_transaction` —
makes the COPY fallback (and all bulk INSERT VALUES) ~an order cheaper.

### B3: parse-cache pollution guard
`lib.rs:10284`: skip `parse_cache.put` for statements > 4KB (stops 25KB-key AST
deep-clones evicting hot entries).

### B4 (separate PR, correctness): session-transaction COPY
When `session_in_transaction(session_id)`, route COPY rows through
`insert_validated_tuples_in_transaction` on the open session txn so
`BEGIN; COPY; ROLLBACK` rolls back. Visible behavior change; wire tests required.

## Invariants to preserve

- Crash mid-COPY exposes zero rows (single WriteBatch commit; version keys share one commit_ts).
- Triggers keep firing (gate → fallback path unchanged).
- Durability semantics identical to DML: `durable_commit=true` ⇒ exactly one cohort fsync per COPY.
- ART/HNSW maintained post-batch (in-memory, restart-rebuilt) — same as existing batch path.

## Tests (must add — zero e2e COPY coverage exists today)

- Wire-level (harness `src/protocol/postgres/wire_tests.rs:17`): COPY text + CSV happy
  path; NULLs/escapes/quotes; duplicate-PK rejects whole COPY (0 rows); trigger-table
  falls back and fires triggers; FK/CHECK table falls back; `COPY n` tag correct;
  error-mid-COPY leaves 0 rows.
- psycopg (`tests/protocol_tests/`): `copy_expert` CSV round-trip ≥100k rows + count check
  (wire-path rule from campaign doc).
- B4: `BEGIN; COPY; ROLLBACK` → 0 rows; `BEGIN; COPY; COMMIT` → n rows.

## Gate (campaign §Milestone gate)

Scalability criterion: COPY 100k ≤ 0.5 s (≥4.6×); 10k/50k proportional; SELECT 1 +
indexed-read sweeps within ±5% of baseline; lib tests 1915/0; protocol suite green;
`ci_perf_smoke.sh` green.

## Rollback

Single revert of the branch merge; the fallback SQL path stays intact underneath (gates
short-circuit to it), so reverting B1 cannot strand data formats.
