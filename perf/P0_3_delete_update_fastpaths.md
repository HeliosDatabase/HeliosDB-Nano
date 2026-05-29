# P0#3 — DELETE fast path + UPDATE expression-RHS fast path

**Branch:** `perf/p0-p1-p2`

## UPDATE with expression RHS — already satisfied ✓ (verified)

`try_fast_update` already evaluates simple expression RHS via
`fast_eval_simple_expr` (handles `col +|-|* literal` for the int types). Measured
on a clean table (in-memory, `run_scaling_diag`):

| rows | UPDATE `v = <lit>` | UPDATE `v = v + 1` |
|---:|---:|---:|
| 2,000 | 44.4 µs | 42.6 µs |
| 8,000 | 43.0 µs | 41.9 µs |
| 32,000 | 45.3 µs | 47.1 µs |

Expression RHS is **identical** to literal RHS and **flat (O(1))** — the fast path
fires. (The earlier 400 µs seen for `update_by_pk` on the wide `users` table was
row-width + warm result/row-cache invalidation overhead on a 5-column TEXT row,
not a fast-path miss — confirmed by this clean single-table measurement.)

## DELETE — already O(1), and fast on disk after P0#2

DELETE by PK was the O(n) full-scan bug fixed in the baseline
(`get_referencing_fks`, 118× at 32k rows) — it is now O(1) (~78 µs in-memory,
flat). On disk, P0#2 removed the per-statement fsync, taking it from 63/s to
4,175/s.

## Dedicated DELETE fast path — scoped follow-up (not landed this round)

DELETE still runs the full parse → plan → `get_referencing_fks` →
`get_row_by_pk` → commit pipeline (no parse/plan-skipping fast path like
`try_fast_insert`/`try_fast_update`). On disk this leaves it slower than UPDATE
(239 µs vs 47 µs) because UPDATE hits `update_tuple_fast`.

A `try_fast_delete` for `DELETE FROM t WHERE pk = <literal>` (bail on triggers /
RLS / referencing-FK / branch, else `get_row_by_pk` + direct delete + ART
maintenance, mirroring `update_tuple_fast`) would close that gap (est. disk DELETE
≈ 2–5× → UPDATE parity).

**Deliberately deferred this round** because its dispatch belongs in
`execute_in_transaction_inner`'s fast-path block — the exact function the
concurrent insert-path session is actively editing (`try_fast_insert*` routing) —
so landing it now would create an avoidable `lib.rs` merge conflict. It is a pure
optimization (DELETE is already correct and O(1)), safe to land after the insert
work merges. Tracked in the consolidated report.

## Status

- UPDATE expression-RHS fast path: **done/verified** (baseline `fast_eval_simple_expr`).
- DELETE: **O(1) + disk-fast** (baseline `get_referencing_fks` fix + P0#2).
- Dedicated `try_fast_delete`: **scoped follow-up** (merge-ordering with insert work).
