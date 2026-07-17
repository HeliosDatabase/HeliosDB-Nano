# W3.5 — Version-aware tuple decode for snapshot reads after ALTER

Status: **STAGE 1 IMPLEMENTED (decode-side NULL-pad).** Base for the design was
`perf/w3-design` off v4.2.0 (`a2e1b5b`); Stage 1 landed on `perf/w3-impl-2026-07-17`
off v4.3.0 (`e4d26c7`). Filed during the W2 gate as the follow-up to
`w2_5_watermark_read_tests::alter_add_column_hidden_from_open_snapshot_reader`.
Companion analysis: `PERF_ANALYSIS_2026_07_13.md`; spec `WAVE_IMPL_SPEC_2026_07_16.md`
§W2.5 (the watermark that fail-closes ALTER to the snapshot path — the leak direction
this item inherits already fixed).

> **What shipped (Stage 1).** The base-table scan decode boundaries (the
> `RowDecodeHint::Full` arm of `scan_table_with_schema_opt`, the version-resolved decode
> in `scan_table_at_snapshot`, and both arms of `scan_table_branch_aware_with_schema`)
> now shape a decoded base row to the current catalog width: a row SHORT of the width
> (post-`ADD COLUMN`) is NULL-padded; a row WIDER than the width (post-`DROP COLUMN`) is
> TRUNCATED to it. **Corrected during adversarial review (was: wider = hard error):** a
> wider row is a legitimately-old/forked version, not corruption — `drop_column_from_rows`
> rewrites `data:` in place with no new version, so a pre-DROP snapshot resolves the wider
> old version and a `bdata:` fork made before a main DROP is left un-rewritten; hard-erroring
> those regressed correct pre-W3.5 time-travel/branch reads and the DROP crash-recovery read
> (a durable wide `data:` under a narrowed catalog) into an "unreadable table" error.
> Truncation reproduces the pre-W3.5 projection exactly (the plan, built from the current
> catalog, only references the surviving indices, so trailing values were already ignored).
> Gated by `[storage] snapshot_schema_evolution` (`"null_pad"` = Stage 1, **default**;
> `"strict"` = the pre-W3.5 arity error, kept as a rollback; `"versioned"` = Stage 2,
> RESERVED and rejected at config parse). The three error-shape characterization tests now
> assert the Stage-1 `Ok(c IS NULL)` flip; `wider_than_catalog_row_truncates` asserts the
> DROP/wider truncate-to-Ok; `branch_before_alter_sees_main_backfill_today` stays pinned
> (branch-model, §7). The shaper is localized to the scan boundary per §1.1 — the generic
> evaluator/aggregate/join arity guards are UNCHANGED and stay planner-bug detectors.
>
> **What remains (follow-ups).**
> - **Stage 2 — catalog time-travel (§3.1, §4b):** resolve the schema as-of the snapshot
>   (`W_snap`) and build the plan / `RowDescription` against it for full PG column-set
>   parity. Not implemented; `snapshot_schema_evolution = "versioned"` is reserved for it.
> - **COPY/vmeta backfill leak (§6):** Stage 1 does NOT cover COPY-inserted rows. A COPY
>   fast-batch row carries no per-row version; after an ALTER in-place rewrite its current
>   `data:` is the 3-value backfilled tuple, so a pre-ALTER snapshot / AS OF read resolves
>   `c = 42` directly — a value leak, not an arity one (nothing to pad). Closed only by
>   Stage 2 (truncate to `W_snap`) or the `add_column_to_rows` pre-image write (§6).
> - **Per-branch schema pin (§7):** an un-forked branch row still reads main's live
>   rewritten `data:` (sees the backfill); insulating a pre-ALTER branch from a main ALTER
>   is out of tuple-decode scope.

---

## 1. The defect, precisely

`ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42` rewrites every `data:{t}:` row in place —
`add_column_to_rows` (`engine.rs:7964`) reads each stored tuple (2 values), pushes the
literal default (`:8028`), and re-`put`s a 3-value tuple with **no** snapshot-resolvable
version. The caller `catalog::update_table_schema` (`catalog.rs:326`) persists the new
3-column schema and `bump_schema_generation`s (`:351`); `add_column_to_rows` itself
`invalidate_write_watermark`s before the first overwrite (`:7983`) and bumps again after
(`:8051`). W2.5's watermark is therefore fail-closed: an in-transaction reader whose
fast-read gate was open on `t` drops to the snapshot path (`scan.rs txn_base_tuples:179`,
default-closed branch at `:188-200` falls through to `scan_table_at_snapshot` at `:201`).
**The leak direction — a fast-read serving the rewritten 3-value rows — is closed.** What
remains is what the snapshot path *returns*.

On the snapshot path, `scan_table_at_snapshot` (`engine.rs:11542`) discovers the row IDs,
then `read_at_snapshot` (`time_travel.rs:706`) resolves, per row, the latest version
`<= snapshot_ts`. For a row inserted before the reader's snapshot that is the **2-value
pre-ALTER version** (the ALTER wrote none). The tuple is `bincode`-deserialized at its
stored arity (`engine.rs:11590`) — **no padding**. Meanwhile the query plan was built
against the **current** catalog: `SELECT *` expands `Wildcard` to one explicit
`LogicalExpr::Column` per current column (`planner.rs:2158-2167`), i.e. 3 columns
including `c`. Projecting column index 2 over a 2-value tuple fails:

- direct path: `project.rs:131` `tuple.get(idx)` → `None` → `"Column index 2 out of bounds in tuple"`;
- evaluator path: `evaluator.rs:273`/`:282` (`Column`/`BoundColumn`) → the same message.

**Provenance (verified — these guards predate this campaign).** `evaluator.rs` Column
guard: commit `484460e` (2026-02-04); `BoundColumn` guard `3e5f4944` (2026-06-11);
`project.rs:133` is the same idiom. All predate Wave 2 (2026-07). The error is not a W2/W2.5
artifact; W2.5 only routed the read to the path that surfaces it.

### 1.1 Why blanket NULL-padding was rejected (the load-bearing constraint)

The identical `"out of bounds in tuple"` guard fires on **intermediate** tuples produced by
a mis-planned query — not just base rows. The same guard family lives at
`aggregate.rs:571`/`:679` (aggregate input / GROUP BY), `join.rs:99` (join output), and
`project.rs:133` (post-aggregate projection). It caught a real planner defect: the
ORDER-BY-over-grouped-plan bug fixed in `e8e905d` (checklist v334_t8) sliced post-aggregate
aliases positionally and referenced a group key that had no alias in the intermediate
schema — a wrong-arity/wrong-index over an **aggregate output** tuple. Blanket-padding
*any* short tuple to *some* width would mask exactly this class. **The fix must therefore
be localized to the base-table scan boundary, where the tuple is provably a stored row of a
known table with a known current schema — never the generic evaluator/aggregate/join
guards, which must stay hard errors for intermediate tuples.**

### 1.2 The distinction the decode must draw

At the **base-table scan decode** only:

- **Legitimately-old-schema row** — stored arity `A < W_now` (current catalog width) AND
  the shortfall is explained by columns added after the row was written (the table *grew*):
  → shape to the snapshot's column set (pad the added columns with **NULL**, see §3).
- **Executor/planner wrong arity** — a reference to column index `>= W_now` (a column that
  does not exist even in the current catalog), or a short tuple at any intermediate
  operator: → **stay a hard error.** Detectable without any per-tuple version: index
  `>= W_now` is a plan/catalog contradiction independent of storage.

`W_now` is known at scan time (the catalog schema the scan already receives —
`scan_table_with_schema(table, schema)`), so the base-scan can pad-by-arity to `W_now`
with zero new metadata. The generic guards never see a short base tuple and remain pure
planner-bug detectors.

---

## 2. Where schema generations are stamped today — and why they don't suffice

`schema_generation` is a process-local `AtomicU64` (`engine.rs:1847`), read at `:2568`,
bumped at `:2576`. Bump sites (all catalog/branch mutation choke points, so every
interface is covered):

| site | file:line | event |
|------|-----------|-------|
| `create_table` | `catalog.rs:260` | table appears |
| `update_table_schema` (**every ALTER funnels here**) | `catalog.rs:351` | column shape changes |
| `drop_table` | `catalog.rs:396` | table disappears |
| `rename_table` | `catalog.rs:1250` | name changes |
| `create/drop_materialized_view` | `materialized_view.rs:168`/`:234` | MV shape |
| `add_column_to_rows` / `drop_column_from_rows` | `engine.rs:8051`/`:8134` | post-rewrite fence |
| `merge_to_main` (gated `if merge_to_main`) | `engine.rs:8960`/`:9034` | main data replaced |
| `set/clear_current_branch` | `engine.rs:12503`/`:12518` | branch visibility |

Two properties make `schema_generation` **insufficient to resolve a snapshot's schema**:
it is (a) **not persisted** (a fresh `AtomicU64::new(0)` per process, `engine.rs:2308`/`:2536`)
and (b) **not mapped to column counts or timestamps** — it is a monotonic cache-invalidation
epoch, not a schema history. It answers "did the schema change since generation N?" (used by
the W1.3 existence cache and the W2.5 watermark) but not "how many columns did `t` have at
`snapshot_ts`?". That second question needs either a per-tuple stamp (§4a) or a persisted,
timestamp-keyed ALTER history (§4b).

### 2.1 Do tuples carry a schema version today?

**No.** `Tuple` (`types.rs:383`) is `{ values: Vec<Value>, row_id, branch_id }` — the row's
own arity is the only schema signal, and `branch_id` is `#[serde(skip)]`. The on-disk row
blob is a bare bincode value-sequence; its length *is* the write-time column count, but
there is no generation/version tag. This is the crux: the decode can see "this row has 2
values, the catalog has 3" but cannot, from the row alone, distinguish "written under a
2-column schema" (pad) from "corrupt / planner bug" (error) — hence §1.2's rule keys on
`W_now`, not on a per-tuple stamp.

---

## 3. Design (staged): shape base tuples at the scan boundary

### Stage 1 — decode-side NULL-pad to current catalog width (cheap, no format change)

**Change.** At the base-table scan decode — the `RowDecodeHint::Full`
arm of `scan_table_with_schema_opt` (`engine.rs:6982`/`:6994`), the version-resolved decode
in `scan_table_at_snapshot` (`engine.rs:11590`), and the two branch-overlay decodes in
`scan_table_branch_aware_with_schema` (`engine.rs:12716` main, `:12765` bdata) — shape a
decoded tuple to `schema.columns.len()` (`Vec::resize`): pad `A < W` with `Value::Null`, and
**truncate** `A > W`.

**Wider is NOT corruption (corrected during adversarial review).** The original design
hard-errored `A > W` on the premise that a row wider than the catalog is corruption. That is
false under `DROP COLUMN`: `drop_column_from_rows` (`engine.rs:8132`) rewrites `data:` in
place with **no** new snapshot-resolvable version and does **not** touch a branch's `bdata:`,
so a pre-DROP snapshot / `AS OF` legitimately resolves the wider old version, an open txn
reading across a concurrent DROP sees it, and a `bdata:` fork taken before a main DROP stays
wide. Erroring there converted correct pre-W3.5 reads into a hard "corruption" error (and,
on the `Full` current-read path, made a table unreadable in the DROP crash window between the
durable narrow-catalog write at `lib.rs` `update_table_schema` and the row rewrite). The
correct shaping is **truncate to `W`**, which reproduces the pre-W3.5 projection exactly: the
plan is built from the current catalog and only references indices `< W`, so the trailing
values were already silently ignored (`Tuple::get(idx)` is `Some` iff `idx < len`). The only
genuine wrong-arity — a plan referencing a column index that does not exist even in the
current catalog — is an INTERMEDIATE-tuple concern caught by the generic
evaluator/aggregate/join guards (§1.1), which this base-scan shaper never touches.

**This is not a new idiom — it is the existing one made consistent.** The projected-decode
paths already do exactly this: `decode_tuple_prefix`/`decode_tuple_columns`
(`prefix_decode.rs:150`/`:163`) build `vec![Value::Null; total_cols]` (`:102`) and fill only
the stored values, so `SELECT c FROM t` (a `Columns` hint) *already* returns NULL for a
short row while `SELECT *` (the `Full` bincode path) errors. The storage-mode resolution loop
already tolerates short tuples (`engine.rs:7007-7010`, `if idx >= tuple.values.len() { break }`).
Stage 1 removes the `Full`-vs-hint inconsistency.

**Result semantics.** `SELECT *` on an open snapshot returns the new column as **NULL**, not
the DEFAULT `42`. This is the W2.5 test's already-anticipated `Ok(c IS NULL)` branch.

**Does Stage 1 project the new column?** Yes — as NULL. It does **not** achieve strict PG
"the snapshot does not see the column at all" (§3.1), because the plan is still built against
the current catalog (3 columns). It is the correct, isolation-preserving *approximation*:
the reader never observes the post-snapshot backfill value (§3.2).

### 3.1 Stage 2 — catalog time-travel (full PG parity)

Resolve the table's **schema as-of `snapshot_ts`** (`W_snap` columns, §4b) and build the
plan / extended-protocol `RowDescription` against `W_snap`, so:

- `SELECT *` on a pre-ALTER snapshot yields the **pre-ALTER column set** (2 columns) — the
  new column is not projected at all;
- an explicit reference to the not-yet-existing column errors as *"column `c` does not
  exist"* at planning, keyed to the snapshot schema (PG raises `42703` for a column absent
  in the snapshot);
- in-place-rewritten rows whose stored arity `> W_snap` (the COPY/vmeta case, §6) are
  **truncated** to `W_snap`.

Stage 2 is the larger change (thread a snapshot-resolved schema through planning and Describe);
Stage 1 is the shippable correctness floor that removes the error and the isolation violation.

### 3.2 Why NULL, not the DEFAULT backfill (justification)

PostgreSQL snapshot isolation: an open snapshot that predates the `ADD COLUMN` must not
observe any effect of the transaction that added the column — including its backfilled
DEFAULT. The `42` is a value written by a transaction that committed **after** the reader's
snapshot; surfacing it is a phantom read of post-snapshot state and violates
RepeatableRead/Serializable. Nano cannot (pre-Stage-2) hide the column entirely, so Stage 1
projects the SQL "absent/unknown" value **NULL** — never `42`. This is strictly more correct
than today (an error is not a valid row; a leaked `42` would be an isolation break); it is
the conservative direction the W2.5 alter test already sanctions.

---

## 4. Where the generation stamp would live (two options, cheaper one recommended)

### 4a. Stamp tuples (rejected as primary — on-disk cost + migration)

Add a write-time `schema_generation` (or column-count) to the row blob. On-disk format
change ⇒ a **forever fallback deserializer** per the W2.2(b) precedent
(`SnapshotMetadataLegacy`, `time_travel.rs:66-89`: parse the new shape, fall back to the old,
never remove the fallback): an un-stamped blob = "pre-stamp row, resolve by arity against the
current catalog" (= Stage 1). Costs +8 bytes/row write amplification and touches every write
path. **Not recommended** — it duplicates information the arity already encodes for Stage 1,
and Stage 2 needs a *timestamp→width* map that a per-row stamp cannot provide (a row stamped
"gen 5" still can't answer "width at snapshot_ts" without the gen→ts→width table anyway).

### 4b. Resolve via the catalog's ALTER history (recommended for Stage 2)

Persist, per table, a small **ordered log of column-count changes** keyed by the logical
commit timestamp (`next_timestamp()`, the same clock `read_at_snapshot` compares against) at
which each `ADD`/`DROP COLUMN` committed — e.g. `schemahist:{table}` →
`[(ts0, ncols0), (ts1, ncols1), …]`, appended in `update_table_schema` (`catalog.rs:326`,
which already runs on every ALTER and already has the new `Schema`). `W_snap` =
the `ncols` of the latest entry with `ts <= snapshot_ts` (binary search). This is
**cheaper than 4a**: one small append per ALTER (ALTERs are rare), no per-row cost, no row
format change. It reuses the exact clock the snapshot machinery already uses, so it composes
with AS OF, the watermark, and the version index without a new time domain.

**On-disk compat (W2.2(b) shape).** Store the log under a table-metadata key with a forever
fallback: **absent log** (a database written before this lands, or a table never ALTERed) ⇒
"no evolution recorded" ⇒ `W_snap = W_now` ⇒ Stage-1 pad-by-arity to the current width.
Never remove the fallback. If only Stage 1 ships, there is **no format change at all** — the
padding is pure decode-side and old data reads unchanged. Mixed-version replication: the log
is additive metadata; a standby that does not understand it falls back to `W_now` (Stage 1),
which is safe (never leaks the backfill, never errors on a legit old row).

---

## 5. Interaction with time-travel AS OF (same problem class — shared fix)

**Yes, AS OF before an ALTER already misbehaves identically.** The AS OF branch resolves the
clause to a snapshot ts (`resolve_timestamp`, `time_travel.rs:445`, wall-clock string →
nearest registered snapshot's logical ts) and calls the **same** `scan_table_at_snapshot`
(`scan.rs:2244`) against a plan built from the **current** catalog. So
`SELECT * FROM t AS OF TIMESTAMP '<before the ALTER>'` resolves the 2-value pre-ALTER
versions and errors at projection index 2 — the exact §1 mechanism, no session transaction
required (pinned by `as_of_read_predating_alter_today`, §8).

This is a **design advantage, not a second problem**: the in-txn snapshot read and AS OF
funnel through one decode boundary (`scan_table_at_snapshot`). Shaping base tuples there
(Stage 1) or resolving `W_snap` there (Stage 2) fixes **both at once** — there is no separate
AS OF path to patch. The characterization test pins that today they fail identically, so the
coordinator can confirm one fix flips both.

---

## 6. Interaction with COPY / vmeta rows (the case Stage 1 does NOT cover)

A COPY fast-batch row carries no per-row `v:`/`v_idx:`; its insert timestamp lives in a
`vmeta:` range marker. In `read_at_snapshot_uncached`, `has_any_versions` is false for such a
row, so it consults `copy_markers.covering_ts` (`time_travel.rs:805`): if
`snapshot_ts >= marker_ts` the row is visible and, having no versions, is read **directly
from current `data:`** (`:817`). After an ALTER in-place rewrite, that current `data:` is the
**3-value backfilled** tuple. So for a COPY-inserted row an open-snapshot / AS OF read
resolves the rewritten `c = 42` — a **backfill leak**, not an error and not NULL (the
opposite failure from the versioned-row case).

- **Stage 1 does not fix this**: the stored arity is already 3 = `W_now`, so there is nothing
  to pad. The leak is a *value* divergence, not an *arity* one.
- **Stage 2 fixes it**: `W_snap = 2` for the pre-ALTER snapshot ⇒ truncate the 3-value row to
  2 columns ⇒ the reader never sees `c`. This is the strongest argument for Stage 2 over
  Stage-1-only.
- **Alternative (engine change, out of design-first scope)**: `add_column_to_rows` records a
  pre-image version (or materializes the covering `vmeta:` markers into per-row versions)
  before the in-place rewrite, so `read_at_snapshot` resolves the 2-value pre-image instead
  of falling through to current `data:`. This is a write-path change with its own cost — a
  bounded follow-up, flagged, not this item.

**Open question for the coordinator (§9):** whether COPY-table ALTER-under-open-snapshot
correctness is required for the target workloads, or whether Stage 1 (versioned-row rows
correct; COPY rows still leak) is an acceptable interim. The characterization test does not
force a COPY row (to stay deterministic), but §8 documents the case.

---

## 7. Interaction with branch overlays

`scan_table_branch_aware_with_schema` (`engine.rs:12663`) overlays `bdata:{branch}:{t}:` rows
(`:12765`) on top of main's **live** `data:{t}:` rows (Step 1, `:12699-12722`) — both decoded
at stored arity, **no padding** (`:12716`/`:12765`). Two sub-cases when a branch was created
before a main ALTER:

- **Un-forked row** (branch made no write): the overlay reads main's **current** `data:` —
  after the ALTER, a 3-value backfilled tuple matching the 3-column catalog ⇒ the branch
  **sees `c = 42`**, no error. This is not an arity problem (3 values = `W_now`); it is the
  branch model — a copy-on-write overlay is **not** snapshot-isolated from main DDL, it reads
  main's live storage for rows it hasn't forked. Insulating a pre-ALTER branch from a main
  ALTER is catalog-versioning scope (a per-branch schema pin), out of this item; today's
  behavior is pinned by `branch_before_alter_sees_main_backfill_today` (§8).
- **Forked row** (branch wrote the row before the ALTER, so `bdata:` holds a 2-value tuple;
  the ALTER rewrote main's `data:` but **not** `bdata:`): the overlay yields a 2-value row
  under the 3-column catalog ⇒ **the §1 arity error** on the branch read path. Stage 1's
  base-scan padding, applied to the `bdata:` decode (`:12765`) too, fixes it (→ `c` NULL);
  pinned by `branch_forked_row_before_alter_today` (§8).

So Stage 1 must pad in **all three** base decodes (main `Full` scan, `scan_table_at_snapshot`,
and both arms of the branch overlay) for uniform behavior; the branch un-forked "sees the
backfill" case is explicitly out of Stage-1/Stage-2 tuple scope and left to a future
per-branch schema pin.

---

## 8. Deliverable 2 (shipped): characterization tests

`tests/w3_5_alter_snapshot_characterization.rs` (w2_5 style, in-memory DB, time-travel on by
default — `config.rs:484`). Each test **asserts today's exact behavior** and comments which
assertion the design flips:

| test | shape | today (pinned) | design flips to |
|------|-------|----------------|-----------------|
| `open_txn_read_after_alter_add_column_today` | open RepeatableRead txn, snapshot predates ALTER, re-read `SELECT *` | `Err` `"out of bounds"` (2-value version under 3-col plan); never the `42` backfill | `Ok` with `c IS NULL` (Stage 1) / 2-col result (Stage 2) |
| `as_of_read_predating_alter_today` | autocommit `SELECT * … AS OF TIMESTAMP '<before ALTER>'` | `Err` `"out of bounds"` — same mechanism, no session | same as above (shared fix, §5) |
| `branch_forked_row_before_alter_today` | branch forks a row pre-ALTER; main ALTERs; branch `SELECT *` | `Err` `"out of bounds"` (2-value `bdata:` under 3-col plan) | forked row `c IS NULL` (§7) |
| `branch_before_alter_sees_main_backfill_today` | branch, **no** fork; main ALTERs; branch `SELECT *` | `Ok`, 2 rows, `c = 42` (overlay reads main's live rewritten `data:`) | **unchanged** by Stage 1/2 (branch-model, §7); a future per-branch schema pin would flip it |

The error-shape tests use the w2_5 defensive `match` (assert the `"out of bounds"` message in
the `Err` arm — today's path — and, in the `Ok` arm that only the fix reaches, assert the
invariant that survives the flip: **no row ever exposes the `42` backfill on a pre-ALTER
snapshot**). The branch-backfill test asserts firmly (deterministic `Ok`). None change engine
behavior.

---

## 9. Interface coverage & open questions (gate #5)

- **No new knob ships this item.** W3.5's code deliverable is characterization tests only —
  no counter/toggle/instrumentation is added, so gate #5 has nothing to wire here (stated
  explicitly, per the campaign rule that a change with no new tunable says so).
- **Future fix's knob (named, config.example.toml style, NOT wired)** — following the
  W3.1 `hot_shape_slots` / W3.3 `statement_retry_max` precedent (name it, default =
  preserve-today, one-way opt-in, leave the default flip to the coordinator). Home it in
  `[storage]` beside `time_travel_enabled`:

  ```toml
  [storage]
  # How a snapshot / AS OF read decodes rows written under an OLDER schema than
  # the current catalog (after ALTER ADD/DROP COLUMN). One-way opt-in; the flip
  # changes an error into rows, so it is off by default for a staged rollout.
  #   "strict"    = today: error "Column index N out of bounds in tuple" (default)
  #   "null_pad"  = Stage 1: project columns added after the row as NULL
  #   "versioned" = Stage 2: resolve the schema as-of the snapshot (pre-ALTER
  #                 column set; the new column is not projected)
  # Default: "strict"
  snapshot_schema_evolution = "strict"
  ```

  A pure correctness fix arguably should not be gated at all; the toggle exists only to
  de-risk the error→rows flip for any consumer that keys on the current error, mirroring
  W3.2/W3.3's default-off stance. Whether to ship default `"null_pad"` (bug fix on by
  default) or gate it is a coordinator decision.

- **Open questions for the gate:**
  1. Is COPY-table ALTER-under-open-snapshot correctness required (⇒ Stage 2 or the
     `add_column_to_rows` pre-image write, §6), or is Stage 1 (versioned rows correct, COPY
     rows still leak `42`) an acceptable interim?
  2. Should a pre-ALTER **branch** be insulated from a main ALTER (§7 un-forked case)? That
     is a per-branch schema pin, larger than the tuple-decode fix, and today's behavior
     (branch sees main's backfill) is pinned as-is.
  3. Stage 1 alone, or Stage 1 + Stage 2 together? Stage 1 removes the error and the
     isolation break with no format change; Stage 2 adds strict PG column-set semantics and
     closes the COPY leak at the cost of threading a snapshot schema through planning/Describe
     and a persisted ALTER-history log (§4b).
