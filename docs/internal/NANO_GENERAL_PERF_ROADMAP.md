# Nano general-performance roadmap (post-pivot, 2026-06-15)

**Focus (per user pivot):** Nano as a general-purpose OLTP/OLAP/HTAP database — NOT
the code-graph ingest workload (CodeKB pinned to 3.36.1). The benchmark of record is
**pg35 vs PostgreSQL 18.4** (+ the SQLite mirror). Every change stays pg35-gated +
erosion-tracked.

## Where Nano stands (pg35 historical envelope — `perf/v358_program/`)
Nano wins **32–34 of 35** categories with **zero PG wins**. The margin is not close in
most: point ops, DML, DDL, subqueries, aggregates run **30×–3000×** faster than PG.

**The improvement surface is just the 5 lowest-margin categories** (median ratio
nano/pg; <1 = Nano faster):

| Category | median ratio | Nano speed | Headroom |
|---|---:|---|---|
| LEFT JOIN | 0.729 | 1.4× | solid, closest of the "easy" wins |
| INNER JOIN | 0.810 | 1.2× | solid |
| 4-table JOIN | 0.882 | 1.1× | can dip to ~tie on a bad run |
| ORDER+LIMIT | 0.973 | ~1× | oscillates Nano/~tie |
| **Prepared stmts** | 1.024 | ~1× (PG slightly ahead) | the only category PG ~wins |

Everything else is a blow-out win → **general-perf ROI is concentrated in joins,
top-k, and the prepared/extended path.** Lifting these turns "32–34/35" into a clean
"35/35" and widens the closest gaps.

## Prioritized general-perf work (all post-launch; do NOT destabilize 2 days pre-launch)

### Tier 1 — close the boundary categories (highest ROI for the pg35 story)
1. **Joins (INNER / LEFT / 4-table)** — Nano's INLJ already wins, but only 1.1–1.4×.
   Levers: better join-order / build-side selection; the **INLJ streaming operator**
   (deferred polish — currently materializes, hurting large one-to-many); indexed-join
   coverage for more shapes. Target: 4-table JOIN off the ~tie boundary.
2. **ORDER+LIMIT (top-k)** — oscillates at ~1×. Lever: ensure the ordered-index top-k
   fast path fires for the pg35 shape; avoid full sort. Target: stable Nano win.
3. **Prepared / extended path** — the one category PG ~wins. NOT a plan-cache problem
   (Nano already text-caches plans); it's per-Parse protocol overhead + extra frames.
   Lever: reduce per-Parse allocation (PreparedStatement / ParameterDescription); the
   pipelining work (item 6) also helps round-trips. Target: prepared ≥ simple.

### Tier 2 — general OLTP hot-path polish (broad, not category-specific)
4. **engine.rs `Arc<Tuple>` row cache** — point lookups deep-clone the tuple on every
   cache-fill (engine.rs ~7497). Switching the row cache to `Arc<Tuple>` removes a deep
   `Vec<Value>` clone per cache-fill AND per cache-hit → general point-read win (and
   point ops are the most common OLTP op; pg35 `Point lookup` / `PK lookup hot`).
5. **integer-filter scan dedup** — the two near-duplicate integer-filter scan methods
   (engine.rs ~3699/4255) — a maintainability/drift fix, minor perf.

### Tier 3 — opt-in bulk-load (kept, de-prioritized; general feature, not CodeKB-specific)
- embed_batch / FastIngest profile / RocksDB knobs / item-1b / candidate-c are committed,
  opt-in, pg35-neutral → keep as bulk-load features.
- **Candidate-(d)** (ART secondary-index deferral under `bulk_load_mode`) — build only on
  general bulk-load demand. The 18.6× ref-write cost is real but only at bulk-load scale
  into multi-secondary-index tables; it does NOT touch the general OLTP path (pg35 clean).

## Method / guardrails
- Every change: pg35 A/B on a **quiet host** + the per-category erosion tracker
  (`perf/v358_program/pg35_track.py`) — must not drop any category below its historical
  envelope. Tier-1 changes target a SPECIFIC category; verify it improved without
  softening others.
- Pre-launch: **freeze** — no hot-path changes; just a clean quiet-host pg35 run to back
  the published "beats PostgreSQL 18.4 on 32–34/35" number honestly.
