---
title: FK validation optimization — design proposal
authors: Claude on gpc001ca:helios:0 (Opus 4.7, max effort)
sponsor: gpc001ca user (requested 2026-05-19)
status: PROPOSAL — awaiting Nano-team triage
related:
  - ENGINE_REGRESSION_BISECT_v3.28.0.md
  - FK-in-txn fix (CHANGELOG v3.22.1)
  - CHANGELOG.md v3.22.1 (DELETE in-txn fix), v3.28.0 (INSERT/UPDATE FK check), v3.30.0 (Quirk H ART path for non-txn)
priority: P1 (regression closure) + P2 (ecosystem positioning)
---

# Proposal: FK validation optimization — engine, switch, and proxy tiers

## TL;DR

The v3.28.0 `check_fk_constraints_on_write` regression is a *symptom* of a single architectural choice — synchronous, per-write, scan-based FK validation against committed state plus a linear merge of the txn write-set. This proposal lays out **ten** distinct solution shapes (five from production DB systems, five novel / Helios-ecosystem-specific), recommends a **four-tier roadmap**, and proposes a **HeliosProxy fk-cache WASM plugin** as the ecosystem game-changer that pushes FK enforcement out of the hot write path entirely.

**Recommended tiers (ship in order):**

| Tier | Change | Effort | Impact | Risk |
|---|---|---|---|---|
| **T1** (must-ship, fixes regression) | In-txn ART index path with write-set overlay (Option A) | 1-2 weeks | Closes v3.28.0 regression for everyone; no API change | Low — ACID-preserving |
| **T2** (power-user knob) | `SET fk_validation = enforced \| deferred \| audit \| off` session GUC (Options B + C + F) | 1-2 weeks | Bulk ingest paths can opt out per-session; matches PG / MySQL / SQLite ergonomics | Medium — needs careful documentation |
| **T3** (DDL design-time) | `CONSTRAINT … NOT ENFORCED` per-FK (Option G) | 3-5 days | Schema-level opt-out matches SQL:202x + PG18 + SQL Server | Low — well-trodden SQL standard surface |
| **T4** (game-changer) | **HeliosProxy `fk-cache` WASM plugin** with WAL/CDC invalidation (Option E) | 3-4 weeks | FK enforcement moves out of engine hot path entirely; per-tenant caches; proxy aggregates many connections | High novelty — but builds on existing WASM plugin scaffold |

All four are independent and can ship in any order; T1 is the blocker for v3.31.2.

---

## 1. Problem recap

`EmbeddedDatabase::check_fk_constraints_on_write` (added v3.28.0 commit `20169f4`) is called on every INSERT and UPDATE. Each call walks every FK on the table and, for each, calls `check_referencing_rows_exist` (`src/lib.rs:8563`). When `active_txn = Some(_)` — i.e. inside any explicit transaction — the function takes the slow path:

```rust
let base = self.storage.scan_table(table_name)?;          // full scan of the *parent* table
let tuples = if let Some(txn) = active_txn {
    txn.merge_with_write_set(table_name, base)?
} else {
    base
};
for tuple in tuples {                                       // O(parent_size) linear scan
    // match FK columns…
}
```

The Quirk H fix in v3.30.0 (`src/lib.rs:8586`) added an ART index fast path explicitly gated on `active_txn.is_none()` to preserve read-your-own-writes for FKs touching just-inserted parent rows in the same txn. This was a correctness-first decision; the cost was the in-txn O(N×M) regression — 1.15B tuple comparisons for the plugin's 117k ref ingest, ~3.3 ks observed.

**The root question**: how do we get FK validation that is (a) cheap per-write, (b) ACID under read-your-own-writes, (c) configurable for workloads that don't need strict enforcement, and (d) ideally moveable out of the engine for high-throughput multi-tenant deployments?

---

## 2. Solution space — 10 options

### Options from established systems

#### A. **In-txn ART index path with write-set overlay** (PostgreSQL `RI_FKey_check_ins`)

PostgreSQL solves this by routing FK checks through the same MVCC-aware index scan path as user queries. `RI_FKey_check_ins` (in `src/backend/utils/adt/ri_triggers.c`) does an `index_beginscan` on the referenced table's PK or UNIQUE index using the parent_values from the new tuple as the scan key. The snapshot's command counter (`CommandId`) ensures the scan sees rows inserted by earlier statements in the same txn.

**Helios sketch**:
- Take the existing ART index lookup (committed-state, O(log N))
- Layer a write-set overlay: if the txn's pending write-set contains an INSERT of `(parent_key)` → return true; if a DELETE of `(parent_key)` → return false; else trust the index.
- Per-check cost: O(log N + log W), where W = txn write-set size. For N writes in a txn: O(N log N + N log W). At W = 18k symbols (the plugin's hot case), log W ≈ 14 → ~117k × 14 = 1.6M operations vs the current 1.15B. **~700× speedup** on the plugin path; no change for the autocommit fast path.

The author of the v3.30.0 Quirk H fix already anticipated this — the source comment at `src/lib.rs:8581-8585` reads "uncommitted in-txn writes are merged below by the scan-and-filter fallback, so the index path stays ACID-correct for the autocommit / implicit-tx surface that dominates the DELETE / DROP workload." We're just extending the index path to handle the in-txn case via overlay.

**Implementation cost**: 1-2 weeks. The txn write-set already exists (`merge_with_write_set` consumes it); we need a `lookup_in_write_set(table, key) -> Option<WriteOp>` accessor and a small wrapper around `art.index_get_all`.

**ACID guarantee**: equivalent to PG semantics. Read-your-own-writes preserved.

#### B. **Deferred FK validation at COMMIT** (SQL:1992 `DEFERRABLE INITIALLY DEFERRED`, Oracle, PostgreSQL)

Per the SQL standard, a constraint can be declared `DEFERRABLE` and either `INITIALLY IMMEDIATE` (default) or `INITIALLY DEFERRED`. With deferred, validation happens at COMMIT instead of per-write.

PostgreSQL has shipped this since 7.3. Each `RI_FKey_check_ins` invocation queues an `AfterTriggerEventData` if the constraint is deferred; the queue is drained at COMMIT.

**Helios sketch**:
- New constraint metadata flag: `is_deferrable: bool`, `initially_deferred: bool`
- Per-txn pending-validation list: `Vec<(child_table, fk_name, parent_key_tuple)>`
- On INSERT/UPDATE of a child row: if FK is deferrable + deferred → append to list and skip the per-write check
- On COMMIT: drain the list. Build a hashset of parent keys per (parent_table, parent_cols). Run one batched `anti-join` query: `SELECT key FROM <pending_keys> WHERE NOT EXISTS (SELECT 1 FROM parent WHERE parent.col = pending.key)`. Throw on any non-empty result.
- On ROLLBACK: discard the list.

Two flavors of validation at COMMIT:
- **B1 (per-key)**: O(N log N) via ART index lookups. Same total cost as A but amortized at COMMIT instead of per-write — useful when the per-write latency matters (e.g. low-latency OLTP would prefer A; OLAP-style bulk would prefer B).
- **B2 (bulk anti-join)**: a single hash-join pass over the parent table. O(N + M) where M = parent rows. **Asymptotically faster than A for very large N** when N > M (i.e. many writes touching few distinct parents).

**Implementation cost**: 2-3 weeks (DDL parsing for DEFERRABLE, txn-pending-list plumbing, COMMIT-time validator).

**Workload fit**: ETL pipelines, bulk load, code_index plugin.

#### C. **Session-level GUC: `fk_validation = enforced | deferred | audit | off`** (MySQL `FOREIGN_KEY_CHECKS`, SQLite `PRAGMA foreign_keys`)

Every production DB exposes a bulk-load escape hatch:
- MySQL/InnoDB: `SET FOREIGN_KEY_CHECKS = 0` (skips all FK validation for the session)
- SQLite: `PRAGMA foreign_keys = OFF`
- SQL Server: `WITH NOCHECK` per-constraint + `EXEC sp_msforeachtable …`
- PostgreSQL: `SESSION_REPLICATION_ROLE = replica` (skips triggers including RI)

**Helios sketch**:
- New session GUC: `helios.fk_validation`
- Values:
  - `enforced` (default): current behavior + T1 fast path
  - `deferred`: apply B1 / B2 to *all* FKs in the session, regardless of DDL setting
  - `audit`: don't validate at all, but emit a violation event to `pg_log_violations` (new system view) for any child row referencing a non-existent parent — caller can inspect post-commit
  - `off`: skip entirely (caller takes responsibility; required for `pg_dump` restore correctness, schema bootstrap, bulk migration)
- Per-statement override: `INSERT … WITH (fk_check = off) INTO …` (non-standard SQL extension; matches PG's `INSERT … ON CONFLICT` extension pattern)

**Implementation cost**: 1-2 weeks (GUC plumbing + branch in `check_fk_constraints_on_write`).

**Workload fit**: pg_dump restore, ETL ingest, embedded-mode plugins like codekb that are structurally trustworthy.

#### D. **Per-constraint `NOT ENFORCED`** (SQL:202x, PG 18, SQL Server `WITH NOCHECK`)

SQL standard recognized in 202x:

```sql
ALTER TABLE child ADD CONSTRAINT fk_parent
  FOREIGN KEY (parent_id) REFERENCES parent(id) NOT ENFORCED;
```

The constraint is recorded in the catalog (so it's visible for documentation, query optimization, planner hints) but the engine skips validation. PG 18 ships `NOT ENFORCED` for CHECK; FK NOT ENFORCED is in active -hackers discussion.

**Helios sketch**:
- Parse `NOT ENFORCED` on `REFERENCES` and `ALTER TABLE ADD CONSTRAINT FOREIGN KEY`
- New constraint metadata: `enforcement: Enforced | NotEnforced | Advisory`
- In `check_fk_constraints_on_write`: skip the check if `enforcement != Enforced`
- `Advisory` is a Helios extension: same as NotEnforced but the query planner can still use the FK for join optimizations (PG's planner does this even for unenforced FKs — they're hints about cardinality)

**Implementation cost**: 3-5 days.

**Workload fit**: data-warehouse star-schemas where dimension tables are immutable-by-convention; cross-system FKs (parent in another DB); plugin-managed FK paths like codekb.

#### E. **HeliosProxy `fk-cache` WASM plugin** *(user-suggested — ecosystem game-changer)*

The proxy already has a WASM plugin runtime (`HDB-HeliosDB-Proxy-Plugins/README.md`) with `pre_query`, `route`, and `rewrite` hooks. A new `fk-cache` plugin would intercept INSERT/UPDATE statements before they reach HeliosDB and validate FKs against a proxy-side cache.

**Architecture**:

```
┌──────────┐    INSERT       ┌───────────────┐    INSERT      ┌──────────┐
│  Client  │ ──────────────→ │  HeliosProxy  │ ─────────────→ │ HeliosDB │
└──────────┘                 │  + fk-cache   │  (with hint:   │ Nano     │
                             │  WASM plugin  │   skip_fk=1)   │          │
                             └───────────────┘                └──────────┘
                                     │                              │
                                     │ ←── WAL/CDC stream ──────────┘
                                     │      (invalidate on parent
                                     │       INSERT/UPDATE/DELETE)
                                     ▼
                             ┌────────────────────┐
                             │ Per-tenant caches: │
                             │  - bloom filter    │  ← O(1) per check
                             │  - LRU hash set    │  ← exact membership
                             │  - parent key TTL  │
                             └────────────────────┘
```

**Cache design**:

Two-tier per-tenant cache, keyed by `(tenant_id, parent_table, parent_pk_value)`:

- **L1 (Bloom filter, 1-8 MB per tenant)**: O(1) per check. False positive → fall through to L2. False negative impossible. Per-table; refilled from WAL stream on miss.
- **L2 (LRU hash set, capped at 64k entries per table)**: Exact membership. Maintained by the WAL/CDC stream from HeliosDB. New parent INSERTs are added; DELETEs/UPDATEs that change the PK remove the old entry.

**Pre-INSERT hook flow**:

```
function pre_query(ctx):
    if not is_insert_or_update(ctx.sql): return PASSTHROUGH
    fks = catalog.get_fks(ctx.table)      // cached schema, refresh on DDL CDC
    for fk in fks:
        parent_key = extract_key_from_sql(ctx.sql, fk.cols)
        if parent_key is None: continue   // NULL → PG MATCH SIMPLE pass
        if not bloom.maybe_contains(fk.parent_table, parent_key):
            // Definitely not — but might be in our pending tx
            if not l2.contains(fk.parent_table, parent_key):
                return REJECT(fk_violation_message)
        // L1 says maybe, L2 not consulted yet
        elif not l2.contains(fk.parent_table, parent_key):
            return PASSTHROUGH  // let engine verify, plugin adds skip_fk=0 hint
    // All FKs pass: tell engine to skip
    return PASSTHROUGH_WITH_HINT(skip_fk=1)
```

**Engine-side complement (small):**

HeliosDB grows a per-session GUC `helios.fk_validation_source = engine | proxy`. When set to `proxy`, the engine trusts the proxy's `skip_fk` hint (transported via a new wire-level option or a `SET LOCAL` per statement). Defaults to `engine` for direct connections that don't go through HeliosProxy.

**Properties:**

1. **Cost shifts from per-write to per-cache-update** — instead of N validations per ingest, the cache is updated once per parent INSERT/DELETE seen via CDC.
2. **Per-tenant isolation** is enforced at the proxy boundary, not the engine. Engine becomes simpler.
3. **Horizontal scale**: proxy fleet shares cache via Redis / DragonflyDB sidecar; cache invalidation is a Redis pub/sub message.
4. **Trust-but-verify safety net**: an opt-in `helios.fk_validation_source = both` mode runs proxy-side fast-path AND engine-side check; engine logs any disagreement as a cache-coherency alarm.
5. **Per-tenant policies**: cache can hold "frequently-validated" tables only, falling back to engine for cold parents. Memory budget tunable per tenant.

**Implementation cost**: 3-4 weeks across two repos:
- `HDB-HeliosDB-Proxy-Plugins/fk-cache/`: WASM plugin (~1500 LoC Rust → wasm32-unknown-unknown)
- `heliosdb-nano`: ~200 LoC for the `skip_fk` GUC + hint plumbing
- WAL/CDC subscription: depends on existing replication path (T2 roadmap item)

**Risks**:
- Cache coherency under partition / proxy restart — needs a "cold start" mode that defaults to PASSTHROUGH (engine validates) until the cache is warm.
- Schema changes (DROP FK, ALTER FK) need CDC invalidation. Doable; already a pattern in the proxy's catalog cache.
- Direct-to-engine bypass: only proxy-routed traffic benefits. This is acceptable — strict-mode customers use direct engine connections.

**Why this is a game-changer**: this is the database analog of TLS termination at the edge — push the policy enforcement to the cheapest point in the stack, let the origin focus on storage. No mainstream RDBMS currently does FK enforcement at the proxy layer; this would be a Helios differentiator.

### Novel / Helios-specific options

#### F. **Eventual-consistency FKs with violation audit** (audit-then-act pattern)

Pure novel: trust the writer optimistically, log violations asynchronously, surface them via a system view.

```sql
SET LOCAL helios.fk_validation = 'audit';
-- INSERTs proceed at autocommit speed
-- Violations land in pg_log_violations:
SELECT * FROM pg_log_violations WHERE constraint_name = 'fk_…' AND txn_id = pg_current_xact_id();
```

The audit view is materialized async by a background task that walks the txn's write log post-commit. Caller can choose to react (e.g. issue compensating DELETEs) or ignore.

**Workload fit**: ML training data pipelines, log/event ingest where the source-of-truth is upstream and FK violations are anomalies to investigate, not block.

**Prior art**: Cassandra's eventually-consistent secondary indexes; Spanner's "tagged read-write" mode.

#### G. **Compile-time FK proof for programmatic ingest** (DSL approach)

For embedded users with known write sequences (codekb plugin, ETL pipelines):

```rust
let pipeline = db.compile_ingest(
    [
        InsertSeq::new(&parent_table).with_keys(&parent_keys),
        InsertSeq::new(&child_table).with_fk_to(&parent_table, &fk_cols),
    ],
)?;
pipeline.execute()?;  // engine statically proved FK satisfaction; skips per-write checks
```

The compiler verifies:
- All parent keys referenced by child rows are present in the parent insert set OR pre-existing in the parent table
- No DELETEs in between that could orphan a child
- Order of operations satisfies FK temporal constraint

Returns a sealed `IngestPipeline` that the engine can execute without per-write FK validation, with the proof recorded in the WAL for post-hoc audit.

**Prior art**: Datomic's "datalog transaction" abstraction; Materialize's "differential dataflow" pipelines.

**Workload fit**: codekb plugin, GraphQL/ORM-driven bulk operations, data migration tools.

**Implementation cost**: 4-6 weeks. Significant but high-leverage for the plugin ecosystem.

#### H. **WAL-piggyback FK validator** (no per-write check, validate against WAL stream offline)

Per-write: skip FK check, record the write in WAL as usual.
Background validator: subscribes to the local WAL stream, replays writes into a state machine that tracks parent/child sets, emits violation events asynchronously.

**Difference from F**: F runs the audit at commit time; H runs continuously and can catch violations from any source (replication, recovery, direct table modification).

**Implementation cost**: 2-3 weeks. Depends on the existing WAL streaming infrastructure (T2 replication track).

**Workload fit**: best-effort FK enforcement for write-heavy OLTP. Augmentable with backpressure if the validator falls behind.

#### I. **Probabilistic FK with Cuckoo filter per parent table** (novel — fast-path optimization on top of A)

Above and beyond A's index+overlay path: for very hot FK columns, maintain a Cuckoo filter (deletable, unlike Bloom) per-parent-table in memory. Per-check cost: O(1) with ~1% false-positive rate. Fallback to A on Cuckoo positive.

For 117k ref inserts where the parent table has 18k rows, this is essentially 117k × O(1) ≈ 117k operations — another **10× speedup over A** for very-hot paths.

**Implementation cost**: 1 week as an enhancement on top of A.

#### J. **Per-write FK proof carrier** (cryptographic novel — for federation)

For federated / multi-engine deployments: the parent-side engine signs a parent-key existence proof (small Merkle-tree membership proof or BLS signature); the child-side engine verifies the signature on INSERT.

Useful for cross-engine FKs (parent in one HeliosDB, child in another) — currently impossible without distributed transactions. Trades a single signature verification (~µs) for a cross-engine round trip.

**Workload fit**: future Helios federation / multi-region deployments. Not relevant for v3.31.2 but worth tracking as a long-term differentiator.

---

## 3. Recommended tiered roadmap

### T1 — Ship in v3.31.2 (closes the regression)

**Option A: in-txn ART index path with write-set overlay.**

Implementation outline:
1. Add `Transaction::lookup_in_write_set(&self, table: &str, key: &[Value]) -> Option<WriteOp>` accessor (WriteOp = `Inserted(Tuple) | Deleted | Updated(Tuple)`).
2. In `check_referencing_rows_exist`, when `active_txn = Some(txn)`:
   - Compute ART key: `storage::ArtIndexManager::encode_key(values)`
   - Index lookup: `art.index_get_all(&name, &key)` (returns committed row_ids)
   - Write-set overlay:
     - If `txn.lookup_in_write_set(parent_table, key) == Some(Deleted)` and index hit count == 1 → masked out, return false
     - If `txn.lookup_in_write_set(parent_table, key) == Some(Inserted(_))` → return true
     - Else trust index hit count
3. Add a unit test that reproduces the v3.22.1 case (DELETE then DELETE) AND the v3.28.0 case (INSERT child after INSERT parent in same txn) AND the orphan-by-overlay case (INSERT child after DELETE parent in same txn → must FK-violate).
4. Benchmark on the codekb corpus: expect write phase 9.7s → ≤ 12s.

**Acceptance**:
- All existing tests pass.
- New tests cover {insert-then-insert, delete-then-insert, delete-then-delete, update-pk-then-insert-child}.
- Plugin code_index regression closed: `code_index ms write=` returns to single-digit seconds on the test corpus.

### T2 — Ship in v3.32.0 (power-user knob)

**Option C: session GUC `helios.fk_validation`.**

Implementation outline:
1. Register GUC in the existing `GucRegistry` (search for `current_setting` callsites — that's the same plumbing).
2. Branch in `check_fk_constraints_on_write` based on the GUC value:
   - `enforced` (default): existing path
   - `deferred`: append to per-txn pending list, skip per-write
   - `audit`: skip; on detected violation, append to `pg_log_violations`
   - `off`: skip entirely
3. New COMMIT hook: drain the pending list, validate batched via Option B's bulk anti-join.
4. New system view `pg_log_violations` (registered via the SystemViewRegistry — same shape as the v3.31.1 phase 2 migrations).
5. Documentation: a new `docs/guides/fk_validation_modes.md` aligned with the `docs/guides/upgrade.md` pattern.

**Acceptance**:
- `SET helios.fk_validation = 'off'` allows the codekb plugin path to bypass FK checks entirely (write phase drops to ~5s, dominated by RocksDB write throughput).
- `pg_log_violations` populated correctly under `audit` mode.
- `pg_dump` restore documentation updated to recommend `SET helios.fk_validation = 'off'`.

### T3 — Ship in v3.32.0 alongside T2 (DDL design-time)

**Option D: per-constraint `NOT ENFORCED`.**

Implementation outline:
1. Extend `parser::preprocess_*` (or the inline-`REFERENCES` parser path) to recognize `NOT ENFORCED` token after the FK clause.
2. Extend `LogicalPlan::AlterTableAddForeignKey` and the inline `REFERENCES` path to thread `enforcement: ConstraintEnforcement` through to catalog metadata.
3. Add column to internal `_hdb_constraints` table; back-compat default `Enforced`.
4. Branch in `check_fk_constraints_on_write` — skip when `enforcement = NotEnforced`.
5. Optional follow-up: planner uses NOT ENFORCED FKs as join-cardinality hints (matches PG).

**Acceptance**:
- `CREATE TABLE child (parent_id INT REFERENCES parent(id) NOT ENFORCED)` parses and stores.
- `ALTER TABLE child ALTER CONSTRAINT fk_parent NOT ENFORCED` toggles at runtime.
- Catalog-visible: `information_schema.table_constraints` exposes `enforced = 'NO'`.

### T4 — Ship in v3.33.0+ (ecosystem game-changer)

**Option E: HeliosProxy `fk-cache` WASM plugin.**

Phased implementation:
1. **Phase 1 (week 1-2)**: Plugin skeleton with per-tenant Bloom filter + LRU hash set, populated from PASSTHROUGH traffic only (no CDC yet). Each INSERT a child sees → cache miss falls through to engine; engine response observed → cache updated. Cold-start mode is naturally safe.
2. **Phase 2 (week 3)**: WAL/CDC subscription. Plugin opens a streaming replication slot, applies parent-table INSERTs/UPDATEs/DELETEs to its cache. Engine learns a new `REPLICATION_ROLE = proxy_cache` per the existing replication track.
3. **Phase 3 (week 4)**: `helios.fk_validation_source = proxy` GUC on engine side. Proxy injects `SET LOCAL helios.fk_validation = 'off'` before forwarding when cache says "validated". Engine trusts the proxy hint.
4. **Phase 4 (future)**: distributed cache via Redis pub/sub for horizontal proxy scale.

**Acceptance**:
- WASM plugin builds, loads, runs end-to-end against the existing HeliosProxy scaffold.
- 80%+ FK checks served from proxy cache on a steady-state OLTP workload.
- Tenant-isolated: tenant A's cache contents never leak to tenant B's queries.
- Cache invalidation under DDL works (DROP FK invalidates cache entries).

---

## 4. Benchmark matrix

Six workload classes × four candidate fixes (existing 3.30 + T1 + T1+T2 + T1+T2+T3+T4). Sixteen base benchmark runs. Measure: total wall time, p50/p99/p999 write latency, peak memory, allocation count, WAL bytes/s.

### Workloads

| ID | Description | Source | Why it matters |
|----|-------------|--------|----------------|
| **W1** | codekb corpus ingest (694 files / 18 952 symbols / 117 344 refs in one txn) | This regression's repro | The trigger; must close |
| **W2** | KanttBan OLTP — agent register / task INSERT / event cascade UPDATE | `tests/kanttban_quirks_v3_27.rs` (existing) | Verifies T1 doesn't regress OLTP path that v3.28.0 was originally fixing |
| **W3** | TPC-C-style banking transfer — single-row INSERTs, 2 FKs per row, 1000 concurrent sessions | `benches/conflict_detection_bench.rs` shape (existing infrastructure) | High-concurrency OLTP; tests T1 under load and T4's per-tenant isolation |
| **W4** | Bulk DELETE 11k rows from FK-referenced child table (Quirk H's original case) | dashboard-migration triage, Quirk H | Verifies T1 doesn't regress the v3.30.0 fix |
| **W5** | pg_dump restore — `--data-only` for a 100k-row schema with FKs | New benchmark | Validates T2 `fk_validation = off` ergonomics |
| **W6** | Multi-tenant ingest — 8 tenants × W1-shaped corpus in parallel | New benchmark | Validates T4 per-tenant cache isolation + horizontal scale |

### Candidate configurations

| ID | Configuration | Notes |
|----|---------------|-------|
| **C0** | v3.31.1 baseline (current) | The 338× regressed state |
| **C1** | T1 only (in-txn ART overlay) | Expected: closes W1 + W2 regression, neutral on W3-W6 |
| **C2** | T1 + T2 (GUC `fk_validation = deferred`) | Expected: W1 ≈ T1; W5 unblocked; modest W3 improvement under deferred mode |
| **C3** | T1 + T2 + T3 (per-FK NOT ENFORCED) | Expected: identical to C2 for W1-W6; semantic correctness verified by new constraint tests |
| **C4** | T1 + T2 + T3 + T4 (HeliosProxy fk-cache) | Expected: W3 + W6 80%+ improvement; W1 (embedded path) unchanged because plugin doesn't go through proxy |

### Acceptance thresholds

| Workload | C0 (current) | C1 target | C4 target |
|----------|--------------|-----------|-----------|
| W1 (codekb 117k refs) | 3 279 s | ≤ 15 s (200× better) | (same as C1 — embedded path) |
| W2 (KanttBan OLTP) | baseline | ±5% of baseline | ±5% |
| W3 (TPC-C 1k sessions) | unknown | within 10% of baseline | ≥ 50% throughput improvement |
| W4 (Quirk H DELETE 11k) | < 1 s | < 1 s (no regression) | < 1 s |
| W5 (pg_dump restore) | hangs | < 30 s with `fk = off` | (same) |
| W6 (8-tenant parallel) | unknown | unknown (no-scale baseline) | ≥ 4× throughput vs C1 |

### Methodology

- Each (workload, configuration) cell: 3 runs, report median + p99
- Hardware: gpc001ca (build host) and dm26 (canonical engine host) — same CPU class
- Use existing `benches/` infrastructure where possible; net-new benches for W5/W6
- Output: a `benches/fk_validation_matrix_results.md` table committed alongside each tier ship
- Regression gate: any tier whose change degrades a workload by >10% vs baseline blocks ship until investigated

---

## 5. Risks and trade-offs

### T1 (overlay) — low risk

- **ACID hazard**: the overlay must correctly handle the case where the txn deleted the parent then re-inserted it with the same key (write-set has both ops in order). Implementation must walk the write-set in op-order, not as a set. Test coverage on this case is mandatory.
- **ART index staleness**: if the index isn't kept in sync with the catalog (e.g. constraint added without `REINDEX`), the overlay is wrong. The existing code already has this comment ("the index could be stale if the caller registered the constraint without also rebuilding the index") — same risk applies.

### T2 (GUC) — medium risk

- **Foot-gun potential**: `SET helios.fk_validation = 'off'` is destructive (allows orphan rows). Documentation must be loud; ideally limit to superuser / specific role.
- **Audit log volume**: `audit` mode can flood `pg_log_violations` for malicious or buggy clients. Recommend per-session row cap + LRU eviction.

### T3 (NOT ENFORCED) — low risk

- **Catalog migration**: requires a backfill on existing constraints (default to `Enforced`). Standard catalog evolution; no novel risk.

### T4 (proxy cache) — high novelty

- **Cache coherency under proxy restart**: needs a "cold mode" defaulting to PASSTHROUGH-with-engine-check until cache warms.
- **Tenant-cache memory**: 8 tenants × 64k LRU entries × 64 bytes/entry = ~32 MB per proxy. Manageable, but needs eviction policy + per-tenant cap.
- **Direct-to-engine bypass**: strict-mode customers who connect directly to engine lose proxy-level caching. This is acceptable — they get T1 fast path instead.
- **Schema change propagation**: DDL must invalidate cached schema. Proxy already has a catalog cache (per Proxy-Operator design); same invalidation mechanism applies.
- **Wire-protocol extension**: the `skip_fk` hint needs a transport. Options: (a) `SET LOCAL` before each statement (cleanest but adds 1 round trip), (b) extended PG-wire option in StartupMessage / per-Parse (zero overhead but non-standard). Recommend (a) for v3.33; consider (b) for v4.

---

## 6. Out of scope / future

- **Cross-engine federated FKs** (Option J) — interesting long-term but not blocking. v4+ federation track.
- **WAL-piggyback validator** (Option H) — only useful if T1 + T2 prove insufficient. Defer pending real-world feedback.
- **Compile-time FK proofs** (Option G) — high-leverage for plugin ecosystem but ~6 weeks of design + implementation work. Defer to v4 unless a high-value plugin owner picks it up.

---

## 7. Open questions for Nano team triage

1. **T1 implementation**: is `Transaction::lookup_in_write_set` cheap to add? Or does the existing write-set storage need restructuring first? (Affects 1-week vs 2-week estimate.)
2. **T2 GUC plumbing**: does the existing `current_setting` infrastructure support session-scoped GUCs cleanly, or is it all global? (Affects T2's per-session semantic correctness.)
3. **T4 WAL/CDC**: what's the current state of the replication track? Is there a working CDC stream the proxy can subscribe to, or does that need new engine work? (Critical-path dependency for T4 phases 2-4.)
4. **Benchmark infrastructure**: are W1-W6 reasonable to add to the existing `benches/` cargo bench infrastructure, or do W5/W6 need a new harness?

---

— gpc001ca:helios:0 (Opus 4.7, max effort), 2026-05-19
