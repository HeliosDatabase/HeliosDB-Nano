# HeliosDB Nano — deep performance analysis (2026-07-13)

HEAD `cd5b4ee` (v4.1.0 + LIMIT/OFFSET query-normalization merged). Read-only audit:
9 parallel subsystem mappers → dedup/rank → adversarial verification. Full agent output:
`.../tasks/wgbuvieyk.output` + workflow `wf_dcf32ebd-d77`. Constraint on every item: preserve ACID
(WriteBatch atomicity, `durable_commit` contract, snapshot/AS-OF exactness), WAL/replication compat, and
bounded memory.

## Strategic picture
Single-connection **latency is already excellent** — pg35 300-iter vs PG 18.4 is **Nano 34 wins / 1 tie**
(only parity: 4-table JOIN 691µs vs 686µs). The real headroom is elsewhere, in priority order:

1. **Concurrency scaling** — indexed point reads plateau ~48k/s (simple ~168k) at c≥16–64 while PG scales to
   ~100k (`docs/benchmarks/heliosdb-nano-vs-postgresql-2026-07-05.md`). Hard serialization points on the read
   path, worst on the **extended/prepared protocol every production driver uses**.
2. **Bulk / COPY** — ~3× vs PG for unconstrained tables (markers shipped), but **FK/CHECK tables fall off the
   fast path entirely** (~14× slower), and COPY buffers the whole stream in RAM (an OOM vector on this host).
3. **MVCC write amplification** — time-travel on by default: 4 keys + ~7 global locks per DML statement, a
   byte-identical `v:` duplicate of every row, and `version_retention=None` → unbounded growth.
4. **The one join parity category** — normalized multi-joins miss index-nested-loop because parameters don't
   push down.

## WAVE 1 — verified, S-effort, high-confidence (do first; all read-path or gated)

### W1.1 — Unlock extended-protocol autocommit reads (concurrency #1)  ✅ independently verified
`query_plan_with_params` (lib.rs:14990-15005) holds `current_transaction.lock()` across `executor.execute()`
whenever there's no session txn; `query_params_with_columns` (lib.rs:15094-15103) takes it **unconditionally**.
So **every** extended/prepared autocommit SELECT serializes on one global mutex. The simple path already solved
this with the `global_txn_active` atomic fast-out (lib.rs:13207-13214; invariant at 407-413).
- **Fix:** mirror the atomic fast-out — skip the mutex when no global txn is active. ~10 lines.
- **Gain:** ~5–10× at c≥16 on parameterized reads (should track the simple curve 137k→172k).
- **ACID risk:** Low — `global_txn_active` flips only under the same mutex, so a false-negative load is
  linearizable to "locked, found None"; the argument is already accepted in-tree for the simple path.
- **Inc-0:** add `-M extended`/`-M prepared` cells to `bench-engines.sh` point-read; show it barely scales;
  then the fast-out.

### W1.2 — Stop the per-statement LogicalPlan deep-clone (concurrency #8)  ✅ verified
`query_params_inner` deep-clones the whole plan every execution — `(**p).clone()` / `(*parameterized_plan_cached).clone()`
(lib.rs:14939-14941) — only so `apply_rls_to_plan` can own it; a point-read plan is 10–30 allocs. The raw
`query()` path already Arc-executes when `get_current_context().is_none()` (lib.rs:13455-13461).
- **Fix:** back-port the no-tenant Arc-execute; clone only when a tenant context exists.
- **Gain:** 5–15% on extended point reads (compounds with W1.1); cuts malloc-arena pressure at high c.
- **ACID risk:** Low — only hazard is RLS bypass; guard on `get_current_context().is_some()` + RLS tests.

### W1.3 — Kill two RocksDB metadata probes per indexed read (concurrency #7)  ✅ verified
Every indexed point/IN/range probe runs `mv_catalog().view_exists(table)? || !catalog().table_exists(table)?`
(scan.rs:290-291, repeated ~417-423) = 2 RocksDB gets (+ full value fetch/copy) per statement, hitting shared
block-cache shards under load.
- **Fix:** epoch-invalidated existence cache (DDL already funnels through `plan_invalidates_sql_caches`).
- **Gain:** a few µs single-thread; more at c≥16 (removes shared-shard contention on the saturating workload).
- **ACID risk:** Medium-low, staleness-shaped — invalidate on DDL/DROP/CREATE-MV (existing choke points).

### W1.4 — Push `Parameter` predicates so multi-joins use index-nested-loop (join parity #2)  ✅ verified
`can_push_predicate` (rules.rs:1145-1147) matches only `(Column, Literal)`; normalized `WHERE c.id = $1` never
becomes a FilteredScan, so the 4-table join falls to hash/nested-loop (691µs) instead of INLJ. The executor
half (`estimate_index_nested_loop_probe_rows`, param-aware probe) already exists.
- **Fix:** accept `Parameter` in the pushdown gate; move the NULL-exclusion from plan-time to runtime
  (`extract_comparison` treats a NULL param as matching nothing).
- **Gain:** 4-table JOIN 691→~60–120µs (5–10×) — turns the only parity category into a decisive win.
- **ACID risk:** Low, read-path only; the load-bearing detail is correct NULL-parameter semantics at runtime.
- **Inc-0:** `RUST_LOG=debug` the normalized 4-table query, confirm `HashJoin build_phase` fires where the
  raw-literal text plans INLJ (proves the divergence), then relax the gate.

### W1.5 — UPDATE-point 179µs anomaly (write-path #3)  ✅ anomaly real, root-cause pending
pg35 UPDATE point = 179µs vs DELETE point 4.56µs / INSERT single 9.61µs, and UPDATE+subquery 188µs ≈ plain
UPDATE — strong evidence the point UPDATE **misses `try_fast_update_params`** (gates lib.rs:7386-7389; the
RLS/trigger/RETURNING bails there don't apply to the pg35 shape, so the miss is elsewhere in the spec build).
- **Inc-0 (do this first, zero behavior change):** bail-reason counter / `RUST_LOG` on each early-return in
  `try_fast_update_params` / `fast_param_update_spec`; run the exact pg35 UPDATE shape; find the gate.
- **Gain (if a gate gap):** 179→~10–20µs (10–20×) — the single largest untapped single-thread category.
- **ACID risk:** Low — widened fast path must route through the same version-write + WriteBatch as the slow path.

## WAVE 2 — M-effort, high-value

- **W2.1 COPY FK/CHECK fast path (bulk #4).** `fast_literal_insert_spec` returns None on any FK/CHECK
  (lib.rs:6572-6576) → every constrained table (Pagila, a2h targets) drops to the 500-row-SQL generic path
  (~14× slower). Fix: batched FK/CHECK validation via ART probes before the single WriteBatch (atomicity
  preserved by construction). Gain: FK-table COPY 100k ~2.3s→250–400ms. **The dominant real-world COPY cost.**
- **W2.2 MVCC snapshot-bookkeeping diet (mvcc #5).** Collapse the ~7 per-statement global locks to 1–2;
  epoch-micros metadata instead of `Utc::now().to_rfc3339()` String; O(1) snapshot-cache invalidation (today
  a per-write linear scan of up to 1000 LRU entries, time_travel.rs:1042-1069); prefix-bounded scan/recovery
  (`IteratorMode::From(prefix)` at engine.rs:11180 — one-line pure win). Gain 5–15%/DML + relieves the 16T
  write plateau. Metadata still rides the same atomic WriteBatch (durability unchanged); wall-clock→i64 needs a
  versioned/fallback deserialize forever.
- **W2.3 Extended-protocol Parse reuse (wire #9).** Every Parse does a private parse + catalog-execute +
  plan (handler_extended.rs:29,68-69); derive Describe schema from the shared parameterized plan cache instead.
  Gain ~20–50µs/Parse; the second half of closing the concurrency gap for real drivers (pairs with W1.1).
- **W2.4 Streaming COPY decode + bounded peak memory (bulk/stability #12).** `handle_copy` accumulates the
  whole stream into one uncapped `Vec<u8>` (handler.rs:1408-1412) → ~4–6× CSV-size RSS. Stream-decode in
  bounded chunks; keep all-or-nothing via the single `copy_bulk_insert` batch. **Retires a live OOM vector** on
  a host that crashed from exactly this class — the stability mandate makes this high-priority.
- **W2.5 Per-table committed-write watermark for in-txn reads (mvcc #6).** Every scan/point-read inside an open
  transaction routes through `scan_table_at_snapshot`/`read_at_snapshot` (scan.rs:2088,2405) = full-keyspace
  iterate + per-row global-Mutex LRU probe → 30–150× vs autocommit. Serve straight from `data:` when the table
  is unchanged since the txn snapshot. **One-directional snapshot risk** (a missed watermark bump → read sees
  too-new data): bump at the lowest-level `data:` write choke points. Helps every non-autocommit driver
  (psycopg2 default).

## WAVE 3 — larger / higher-risk (design-first)

- **W3.1 Lock-free hot-shape slot + pinned ART probe handles (concurrency #10, backlog #7).** Epoch-validated
  front for the normalized plan cache + resolved point-probe handles, to kill the plan-cache shard mutex + ART
  registry RwLock per statement (the documented c≥32 plateau). Weak-ref + epoch invalidation on DDL/TRUNCATE/
  REINDEX/branch-switch. Medium confidence — needs an off-CPU profile first to attribute the plateau.
- **W3.2 Single-copy latest version (mvcc #13, was R6).** Elide the byte-identical `v:` duplicate on INSERT via
  a flagged `v_idx:` event, materialize-on-first-mutation. Removes 1 of 4 memtable puts + a payload-sized copy
  (+10–25% autocommit insert, up to ~2× write-amp on wide rows). **Highest MVCC risk** (on-disk format
  migration; R6 was deferred for exactly this) — measurement-first, no format change until the win is bounded.
- **W3.3 Same-row statement retry (concurrency #14, backlog #5).** Replace the 1s pessimistic worker-pinning
  spin (lock_manager.rs:186-193; futility documented in-code at 173-185) with engine-internal retry against a
  fresh snapshot (PG-equivalent read-committed). Contended same-row UPDATEs: 1/s → storage-speed. **Highest
  semantic surface** — scope atomicity (retry re-runs the whole statement, never partial), lost-update, and
  deadlock exactly; ship the typed `WriteConflict` error + a contended-writer microbench first.
- **W3.4 ART index maintenance (write/bulk #11).** Per-table entry lists (O(own indexes) not O(all indexes)),
  encode-once, batched tree locking for COPY/bulk. ~0.5–2µs/insert + removes a many-table scaling cliff.
  Measurement-gated with a clean STOP rule (if ART <8% of COPY wall time, skip the batch half).

## Cross-cutting stability items (independent of the above)
- **`version_retention=None` default** (config.rs:483): GC worker never starts, `VACUUM VERSIONS` no-ops →
  unbounded `v:`/`v_idx:`/`snapshot:` growth. Even enabled, the 50k/300s ceiling (~167/s) ≪ ~118k inserts/s.
  Decide a safe default + raise the reclaim ceiling. (Owner decision — surfaced repeatedly.)
- **Legacy logical-WAL GroupCommit** (wal.rs:364,769-786): hardcoded `thread::sleep(10ms)` poll loop; and under
  factory `Sync` mode each logical append fsyncs per statement regardless of `durable_commit`.

## Recommended sequencing
Wave 1 is ~all S-effort, read-path/gated, and hits gaps #1 and #4 — **start with W1.1 + W1.2 + W1.3 together**
(one concurrency PR: mutex unlock + plan-clone + metadata cache, all measured via new `-M extended` bench
cells), then W1.4 (join parity) and W1.5 (UPDATE anomaly instrumentation). Wave 2 leads with W2.1 (COPY FK)
and W2.4 (COPY OOM). Every item is measurement-first with an existing or one-new benchmark tie and a STOP rule.
