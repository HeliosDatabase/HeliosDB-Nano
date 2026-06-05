# ISSUE: secondary indexes orphaned across version upgrade + no usable point-index on UUID columns

**Component:** HeliosDB Nano — storage / catalog / planner (secondary indexes)
**Found:** 2026-06-05 deploying ada-core `library.search` over a 678k-vector /
614k-file corpus, while upgrading the live DB 3.34.0 → 3.37.0 → 3.37.1.
**Severity:** High (A) / Medium (B) — both silently degrade indexed access to full scans.

These are two distinct findings from the same investigation. Both are separate
from the (fixed) `CREATE INDEX hnsw` no-op and the (fixed in v3.37.1) kNN
planner fast-path.

---

## A. Secondary indexes do NOT survive a binary/version upgrade

After swapping the Nano binary to a new version and restarting the daemon on the
**same data-dir**, `pg_indexes` shows **only the `*_pkey` indexes** — every other
secondary index (HNSW vector indexes AND scalar btrees) built under the prior
version is gone / not visible, and queries that relied on them silently fall back
to full scans.

### Evidence (live ada DB, `/home/gpc/heliosdb-ada-data`)
- Under **3.37.0** I built `library_embeddings_hnsw` (CREATE INDEX, backfilled,
  ~1120 s) — confirmed present in `pg_indexes`.
- Upgraded the binary to **3.37.1** (same data-dir) + restart. Afterwards:
  - `SELECT indexname FROM pg_indexes` no longer listed `library_embeddings_hnsw`
    (nor `library_files`'s migration-created secondary indexes: `…_path`,
    `…_status`, `…_extension`, `…_file_id` (mig 0029), etc.) — **only**
    `library_files_pkey`, `library_extractions_pkey`, `library_embeddings`*… were
    gone.
  - `DROP INDEX library_embeddings_hnsw` → "Table 'library_embeddings_hnsw' does
    not exist" (so it's not in the catalog under the new binary).
  - kNN regressed to ~36 s (full scan) until I **rebuilt the index under 3.37.1**
    (then 3 ms). Rebuilding under the running version is the only thing that
    restores it.

### Expected
Indexes built by one released version should remain valid + catalog-visible after
a binary upgrade on the same data-dir (or the upgrade should transparently
rebuild/migrate them). Today an upgrade silently un-indexes the whole DB.

### Fix direction
Persist index structures (or their definitions) in the data-dir in a
version-portable form, and rebuild/validate them on startup after a version
change; or document a mandatory post-upgrade `REINDEX`. ada-core now has to
re-run an index rebuild after every Nano upgrade.

---

## B. No usable point-index on UUID columns (equality full-scans even with a btree)

`SELECT … FROM t WHERE id = $1::uuid LIMIT 1` on a 614k-row table does a **full
scan (~1.7 s)** even with a btree index on the UUID column — the planner never
uses it for equality. (This is why ada-core's migration 0029 added an explicit
`library_files_file_id` index in addition to the PK — but neither the PK nor the
explicit btree is used.)

### Evidence
- `library_files` (614,929 rows), `file_id UUID PRIMARY KEY`:
  - `SELECT path FROM library_files WHERE file_id = $1::uuid LIMIT 1` → **1768 ms**.
  - Created `CREATE INDEX library_files_file_id ON library_files (file_id)`
    fresh under 3.37.1 (built in 1.8 s) → same query **still 1768 ms** (index not used).
- `library_extractions` similarly: `WHERE file_id = $1::uuid` → ~1.7 s (plus
  content_text read).

Net effect: any per-row fetch by UUID over a large table is a full scan. ada-core
worked around it by batching with `WHERE file_id IN (…::uuid literals)` (one scan
for many ids) — but single-row lookups remain pathological.

### Expected
Equality on an indexed (btree or hash) UUID/PK column should be an index probe,
not a full table scan.

### Fix direction
Planner: route `col = $const` (incl. UUID, incl. PK) to the matching btree/hash
index. A hash index type for UUID equality would also help.

---

## Why it matters for ada-core
`library.search` over 678k vectors: the kNN is now 3 ms (v3.37.1 planner fix +
fresh HNSW), but (A) forced a full index rebuild after the upgrade, and (B) made
per-file metadata fetches the new bottleneck (37 s → 5.8 s only after switching to
IN-list batching). Filed from gpc001gb live ops, 2026-06-05.
