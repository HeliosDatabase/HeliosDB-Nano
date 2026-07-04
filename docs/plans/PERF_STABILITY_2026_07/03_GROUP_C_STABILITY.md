# Group C — Stability hardening

**Target:** T3 — panic-free serving path, bounded resources, no silent degradation.
**Source:** stability audit @ 68e814a (14 findings S1-S14 + issue verdicts + flake root cause).
Split into two PRs; behavior-visible defaults are flagged as **DECISION** items (documented,
not flipped unilaterally).

## C-I — `fix/stability-wire-hardening` (Milestone 1 — first PR of the campaign)
Small, low-risk, and de-flakes the release gate for every subsequent PR.

| Item | Fix | Anchor |
|---|---|---|
| **C1** (S1 High) | `MAX_MESSAGE_LEN` (64MB default, config-able) enforced right after reading wire `len`; validate startup-message `bytes_needed` before alloc | `protocol/postgres/messages.rs:267-270`, `handler.rs:464-490,317-330` |
| **C2** (S7 Med) | Bound every count/length in Bind/Parse/CopyData against `buf.remaining()` before loop/alloc; `len<4` CopyData underflow (`copy_to_bytes(len-4)` → usize::MAX, pre-auth reachable); reject, never panic | `messages.rs:302,375-417,407` |
| **C3** (S8 Med) | `TO_DATE`/`TO_TIMESTAMP` non-string format arg: `unreachable!()` → `Err(query_execution)` (client-reachable panic; can poison the S12 mutex via COPY/FK `execute()` path) | `sql/evaluator.rs:1733,1788` |
| **C4** (S12 Low/Med) | `current_transaction: std::sync::Mutex` → `parking_lot::Mutex` (no poisoning; recovery currently inconsistent across `lib.rs:6169` vs `1826/12978`) | `lib.rs:384` |
| **C5** (S5 High) | MySQL listeners get the same `Semaphore(max_connections)` PG already has; wire `--max-connections` to them | `main.rs:814-832,850+` vs `postgres/server.rs:190` |
| **C6** (S11 Med) | Per-conn MySQL prepared-stmt map: cap + LRU evict (PG's bounded LRU at `prepared.rs:91` as pattern) | `protocol/mysql/handler.rs:889,2618,2807` |
| **C7** (S13 Low) | Accept-loop error backoff (EMFILE busy-spin → sleep-with-backoff) on all 3 accept loops | `postgres/server.rs:217-219`, `network/server.rs:105-108`, `main.rs:828-830` |
| **C8** (flake Fix A) | Reorder both HNSW test fixtures: target vector inserted LAST (isolation impossible for non-rank-1 nodes). Reproduced: 1/800 and 4/2000 → 0 | `src/vector/hnsw_index.rs:884-886`, `src/storage/vector_index.rs:1483-1485` |
| **C9** (flake Fix B) | Real recall bug guard: in `HnswIndex::search`, if `results.len() < k.min(live_len())` fall back to brute-force over `id_mapping` (first-inserted high-level nodes can be layer-0-isolated in hnsw_rs 0.3.3 — production under-return, not just a test artifact) | `src/vector/hnsw_index.rs:170` |
| **C10** (Issue B hardening) | EXPLAIN-asserting regression tests for UUID index probes (all 4 combos: PK/secondary × literal/$1::uuid — today only row-count asserted, a full scan passes identically); coerce 16-byte `Value::Bytes` (binary param, untyped OID 0) → `Value::Uuid` in `coerce_index_lookup_value` | `sql/executor/scan.rs:835-839`, tests |

## C-II — `fix/stability-resource-governance` (later milestone, after A/B/D perf groups)

| Item | Fix | Anchor |
|---|---|---|
| **C11** (S4 High) | Default statement timeout + wire `SET statement_timeout` (currently dead code) into a per-session timeout context checked in the executor loop | `config.rs:474-475`, `sql/settings.rs:99-100` |
| **C12** (S10 Med) | Recursive CTE: cumulative row cap + memory cap + `HashSet` dedup (O(n²) `Vec::contains` today); v3.60.9 auto-recursive widened exposure | `executor/mod.rs:3983-4019` |
| **C13** (S9 Med) | WAL replay: stop-at-first-bad-record keeping the good prefix (today one torn record discards ALL logical entries); wire dead `verify_integrity()` into startup | `storage/wal.rs:643,823-909`, `engine.rs:2282` |
| **C14** (S3 High, partial) | Kill the portal remainder double-copy (`results.split_at` + `to_vec` per Execute). Full streaming executor (Vec<Tuple> → cursor) is L-effort — file as follow-up roadmap item, not this campaign | `handler_extended.rs:319-324`, `executor/mod.rs:467` |
| **C15** (Issue A remainder) | Index-def cross-version migration: self-describing encoding (serde_json) for the tiny cold `meta:index:` defs + real version migration path (older-migratable vs newer-unsupported — today ANY version mismatch → index silently dropped, message even mislabels older as "newer"); REINDEX-from-decodable on mismatch | `storage/catalog.rs:474,488-552` |
| **C16** (S14 Low) | Explicit shutdown persist for index snapshots instead of IO-in-Drop | `lib.rs:503-518` |
| **C17** (M1 gate finding) | `helios_sessions` system table: schema defined (`sql/system_tables.rs::helios_sessions_schema`) but never provisioned at startup — protocol suite step 7 fails on every version; wire it up or drop the dead schema + test | `src/sql/system_tables.rs` |

## M5 status (2026-07-04)
C11/C12/C13 SHIPPED (branch `fix/resource-governance`). C11 enforces configured
`statement_timeout_ms`/`query_timeout_ms` server-wide (all paths incl. wire) +
embedded `SET statement_timeout`. Deferred as follow-ups:
- **C11-wire**: wire per-connection `SET statement_timeout` is still accepted-and-
  dropped by the generic-SET compat branch (`handler.rs` ~line 742); needs
  per-session timeout storage threaded through `query_*_for_session`. Config-level
  timeout IS enforced over the wire, so runaway queries are cappable server-wide.
- **C17** (helios_sessions), **C14** (portal streaming), **C15** (index-def version
  migration), **D4** (UPDATE versioning unify), **D6** (pessimistic-lock removal).
| **D4** (from Group D) | Fast/branch UPDATE versioning unification (stale `AS OF` reads today) | `engine.rs:10797-10888,12242-12335` → `time_travel.rs:780-805` |
| **D6** (from Group D) | Drop pessimistic row lock for session-txn writes (1s worker-pinning futile waits → immediate retriable conflict error; doc'd Option 2) | `transaction.rs:448-454`, `docs/NANO_CONCURRENCY_LOCKING.md:55-81` |

## DECISION items (documented, defaults not flipped without owner sign-off)
- **S2**: `durable_commit=false` default — ACKed commits can be lost on power cut. Flipping
  to `true` is the safe default but tanks durable-write TPS on fsync-bound disks and
  changes published benchmark numbers. Proposal: keep default, add loud startup log +
  prominent docs + `--durable` CLI shorthand; revisit as a 4.0 default flip.
- **D5**: `version_retention: None` default — unbounded on-disk version growth;
  `VACUUM VERSIONS` silently no-ops. Proposal: make `VACUUM VERSIONS` return a NOTICE/error
  when retention is off (C-II scope, autonomous), and separately decide a bounded default
  (e.g. 7d) as a product call.
- Note: S6 (COPY buffers whole stream in RAM) is mitigated to O(decoded input) by Group B's
  B1 and bounded fully by B1's follow-up (incremental per-frame decode); tracked there.

## Gate
Standard campaign gate. C-I additionally: fuzz-ish malformed-message tests (truncated
Bind/Parse, `len<4` CopyData, 2GB claimed len → clean error, connection survives),
`TO_DATE('x',123)` over psycopg (error not disconnect), MySQL connection-cap test, HNSW
tests looped ≥2000× (`--test-threads=2` like the release gate). C-I expected perf-neutral:
bench sweep within ±5% on all cells.
