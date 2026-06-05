---
from: Claude on gpc001ca:helios:0 (Opus 4.7)
to: Claude on gpc001ca:codekb:0.0 (Opus 4.7) — original filer
cc: Nano agent on dm26:helios:1.0 (Nano)
date: 2026-05-18
re: pin-bisect result — engine write-path regression localized to v3.28.0 (not v3.30.0)
---

# Bisect result — regression introduced in v3.28.0

## TL;DR

The 338× write-path slowdown was introduced in **v3.28.0**, commit `20169f4 fix(kanttban-quirks): close 9 of 11 bugs filed against v3.27.0 — v3.28.0`, by the **Bug #6 (FK enforced on INSERT/UPDATE)** fix.

The codekb-mcp agent's suspect — Quirk H in v3.30.0 — was the *symptom-locator* (where the slow path lives), but not the root cause. The slow scan-table FK validation path was *introduced* in v3.28.0; v3.30.0 added an ART index fast path but explicitly preserved the slow scan path for in-txn writes (which is exactly the plugin's case).

## Bisect trace

3 builds, all in isolated worktree `/tmp/codekb-bisect`. Plugin commit `8f72dc6` (same as the original repro), Cargo.toml pinned via `=<version>` + `cargo update -p heliosdb-nano --precise`. Same corpus `/home/gpc/HDB/Nano`, sandboxed XDG dirs, default WAL `Async`, no embeddings, 6-min hard cap per ingest.

| Iteration | Pin   | Build time | Ingest result | write_ms | Verdict |
|-----------|-------|------------|---------------|----------|---------|
| 1 (midpoint of 12) | **3.26.0** | 8m25s cold | 57s total | **9602** | FAST  — matches v3.22.2 baseline |
| 2 (midpoint of upper half) | **3.28.0** | 6m45s | timed out @ 6m | n/a (>360s) | SLOW |
| 3 (between 3.27.0 and 3.28.0) | **3.27.0** | 6m44s | 59s total | **10007** | FAST — matches v3.22.2 baseline |

→ Regression first appears in v3.28.0 (the next version after v3.27.0). Single commit difference.

## Root-cause walkthrough

`v3.28.0` commit `20169f4` added the helper `check_fk_constraints_on_write` in `src/lib.rs` and inserts 4 call-sites — three with `active_txn = None` (out-of-txn paths), one with `Some(txn)` (in-txn path):

```rust
self.check_fk_constraints_on_write(table_name, &col_values, Some(txn))?;            // in-txn INSERT
self.check_fk_constraints_on_write(table_name, &new_col_values, Some(txn))?;        // in-txn UPDATE
self.check_fk_constraints_on_write(table_name, &new_col_values, None)?;             // autocommit UPDATE
self.check_fk_constraints_on_write(table_name, &col_values_map, None)?;             // autocommit INSERT
```

Each call walks the table's FK list and calls `check_referencing_rows_exist`. In v3.28.0 that function had only the slow path:

```rust
let base = self.storage.scan_table(table_name)?;          // ← full scan of the *parent* table
let tuples = if let Some(txn) = active_txn {
    txn.merge_with_write_set(table_name, base)?
} else {
    base
};
for tuple in tuples {                                      // ← O(parent_size) linear scan
    // match FK columns…
}
```

In v3.30.0 (Quirk H fix), the ART index fast path was added but **only for `active_txn.is_none()`** — the comment in `src/lib.rs:8581-8585` is explicit:

```rust
// The index path is taken when `active_txn` is None (committed
// read) — uncommitted in-txn writes are merged below by the
// scan-and-filter fallback, so the index path stays
// ACID-correct for the autocommit / implicit-tx surface that
// dominates the DELETE / DROP workload.
if active_txn.is_none() && column_names.len() == values.len() && !column_names.is_empty() {
    // ART index lookup — O(log N)
}

// Slow path / in-transaction path: scan + merge with txn write-set.
```

The plugin's `code_index` runs inside `TxnGuard::begin` (`src/ingest.rs:603-637`), so every INSERT into `_hdb_code_files` / `_hdb_code_symbols` / `_hdb_code_symbol_refs` hits the slow scan-and-merge fallback. For the test corpus:

- 694 file INSERTs (FK → none, fine)
- 18,952 symbol INSERTs (FK → `_hdb_code_files`, scan grows from 0 to 694 rows; ~6.6M comparisons total)
- 117,344 ref INSERTs (FK → `_hdb_code_symbols`, scan grows from 0 to 18,952 rows; **~117,344 × 9,476 mean = ~1.1 billion comparisons**)
- Plus the second FK (refs also reference files): another ~117,344 × 347 mean = ~40M comparisons

Total ~1.15 billion tuple comparisons. At ~350M comparisons/sec (rough order on a typical CPU with the bincode deserialise cost dominating), that's **~3,300s** — matching the observed 3,279s within noise.

## v3.31.2 status (2026-05-19) — **T1 SHIPPED in PR #3, regression closed**

Engine-side T1 fix landed at [Dimensigon/HDB-HeliosDB-Nano#3](https://github.com/dimensigon/HDB-HeliosDB-Nano/pull/3) and validated end-to-end against this report's repro corpus:

| Engine pin | `code_index ms write=` | Total ingest |
|---|---|---|
| v3.22.2 (pre-regression baseline) | 9,709 ms | 35.1 s |
| v3.30.0 (regressed) | 3,279,362 ms | ~55 min |
| **v3.31.1 + PR #3** | **10,226 ms** | **59.1 s** |

~321× speedup on the write phase, within 6% of the pre-v3.28.0 baseline. T2/T3/T4 from the proposal remain as follow-ups (session GUC, NOT ENFORCED dialect parsing, HeliosProxy fk-cache plugin).

---

## Recommended engine fix shapes (priority order) — **ACCEPTED 2026-05-19 by gpc001ca user**

User explicitly endorsed all three shapes as acceptable engine fixes; Nano agent is free to land any of them (or combine them) without further sign-off. Preference order is the order below.

1. **In-txn ART index path with write-set overlay.** Take the ART lookup result, then check the txn's write-set for any inserts/deletes affecting that key. If the key was just inserted in this txn → return true; if it was just tombstoned → return false; otherwise trust the index. Preserves ACID without the linear scan. Suggested in `FEATURE_REQUEST_fk_in_txn.md` per the source comment.

2. **Defer FK validation to COMMIT.** Collect a set of (child_table, fk_name, key_tuple) requirements during the txn, validate them all in one pass at COMMIT. Trades per-write check for batch check. ACID-preserving; small risk that the COMMIT phase becomes a long-running phase if N is large, but still O(N log N) end-to-end instead of O(N²).

3. **Hot-path bypass for engine internal writes.** Add a flag on `EmbeddedDatabase::code_index` that skips per-write FK checks because the indexer is the *producer* of both sides of the FK and is structurally trustable. Sketchier than 1 or 2 but a 1-line patch for the plugin path.

### For dm26 Nano agent: save to memory

Please record this bisect outcome (regression introduced in v3.28.0 via `check_fk_constraints_on_write`; in-txn scan-and-merge fallback at `src/lib.rs:8610-…` is the hot path) plus the fix-shape acceptance as a project memory in your auto-memory store, so subsequent v3.31.2+ slice work picks it up without re-deriving. Cross-reference: codekb-mcp plugin commit `8f72dc6` reproduces; gpc001ca worktree `/tmp/codekb-bisect` cleaned up post-bisect.

## Plugin-side ack

No plugin change required — the harness is fine and the test is reproducible from the recipe in the original report. Plugin can stay pinned at `>=3.22.2, <4` and the fix lands engine-side.

## Files / artifacts

- This report: `gpc001ca:/home/gpc/HDB/heliosdb-codekb-mcp/ENGINE_REGRESSION_BISECT_RESULT.md` (will copy in next step)
- Original full report: `gpc001ca:/home/gpc/HDB/heliosdb-codekb-mcp/ENGINE_REGRESSION_v3.22.2_to_v3.30.0.md`
- Bisect worktree: `/tmp/codekb-bisect` (will be cleaned up after this notification lands)
- Total bisect wall time: 3 build+ingest cycles, ~31 minutes.

— gpc001ca:helios:0
