# W3.2 — Single-copy latest version: byte-duplication quantification + on-disk design

Status: **DESIGN-FIRST. Instrumentation landed; NO on-disk format change ships this
campaign.** Base: `perf/w3-design` off v4.2.0 (`a2e1b5b`). Companion analysis:
`PERF_ANALYSIS_2026_07_13.md` §"WAVE 3" (W3.2), spec `WAVE_IMPL_SPEC_2026_07_16.md` §W3.2.
Prior art this design generalizes: `src/storage/copy_marker.rs` (v4.1.0 COPY version-elision)
and the W2.2(b) forever-fallback deserializer (`storage/time_travel.rs:66-123`).

> **STOP rule (binding).** A single-copy latest version is pursued ONLY if the
> `heliosdb_write_volume` census shows the `v:`/`v_idx:` chain is **≥15% of INSERT
> byte volume**. §1 gives the first-principles estimate (53–72% for the measured
> shapes) and the exact gate command; the go/no-go is a coordinator decision read
> off the counters, not an assumption. The estimate — and the whole win — applies
> ONLY to time-travel-ON configs (§1.6); under the `fast`/`fast_ingest` profiles no
> version bytes are written at all. If the gate contradicts the estimate and the
> share is <15%, record it in §1.4 and deprioritize.

---

## 1. Quantification (the STOP-rule input)

### 1.1 Where the duplicate comes from

Every versioned INSERT writes the row THREE times to RocksDB:

| key family | key                                             | value                     | funnel |
|------------|-------------------------------------------------|---------------------------|--------|
| `data:`    | `data:{table}:{row_id}`                          | serialized row            | `transaction.rs:986` / `engine.rs:10491` |
| `v:`       | `v:{table}:{row_id}:{commit_ts}`                 | **full row copy** (logical value) | `transaction.rs:1024` / `time_travel.rs:1078` / `engine.rs:10779` |
| `v_idx:`   | `v_idx:{table}:{row_id}:{020 reverse_ts}`        | `commit_ts` (8 bytes)     | `transaction.rs:1033` / `time_travel.rs:1082` / `engine.rs:10790` |

The `v:` **value** is the row's **logical** value. For the default uncompressed row-store
it is the same buffer as the `data:` value — the fast path sets `logical_value =
value.clone()` (`engine.rs:10497-10505`) and the commit path passes one `val` to both puts
(`transaction.rs:974` then `:1024`). Under side-storage/columnar
(`schema_uses_column_storage`), however, `data:` holds the transformed sidecar image while
`v:` holds the full uncompressed tuple bincode (`engine.rs:10498-10502`), so the version
value is then **larger** than `data:`, not identical — the census records each on-disk size
independently, and that skew only strengthens PROCEED (more version bytes, not fewer). That
value copy is the target: it is the largest of the three writes and is 100% redundant for a
row that is never read `AS OF` a historical timestamp before its first mutation.

### 1.2 First-principles estimate

For a row with payload `V` bytes, table name `T` chars, row-id `D` digits, `commit_ts`
~16 digits (µs since epoch), reverse-ts zero-padded to 20:

```
data_total    = 6 + T + D + V                       # "data:" + ":" + value
v_total       = 3 + T + D + 16 + V                  # "v:"  + separators + ts + value
v_idx_total   = 7 + T + D + 20 + 8                  # "v_idx:" + separators + revts + 8B value
version_total = v_total + v_idx_total = 54 + 2T + 2D + V
share         = version_total / (data_total + version_total)
```

| shape                     | V   | T | D | version_total | data_total | **version share** | **`v:`-value-copy share** |
|---------------------------|-----|---|---|---------------|------------|-------------------|---------------------------|
| narrow (pg35 `INSERT single`) | 20  | 5 | 4 | 92            | 35         | **72%**           | 16% (V / total)           |
| medium OLTP row           | 120 | 8 | 5 | 200           | 139        | **59%**           | 36%                       |
| wide row (Pagila-ish)     | 500 | 5 | 6 | 576           | 517        | **53%**           | 46%                       |

Every shape is far above the 15% STOP threshold; the `copy_marker.rs` module header
records the same finding empirically ("per-row version traffic was ~2/3 of the COPY cost
on narrow rows", `:5-6`). The *value*-copy alone (the part a single-copy latest version
eliminates outright) is 16–46% of INSERT bytes; the rest is the `v:`/`v_idx:` **keys**,
of which the design keeps the small `v_idx:` event and drops the `v:` key+value. **Provisional
verdict: PROCEED** — pending the gate below confirming it on the census.

### 1.3 How the gate confirms it (coordinator, single heavy-op slot)

```
# any release build — no cargo feature needed
# config.toml: [performance] write_volume_stats = true
psql -c "CREATE TABLE t (id int primary key, v text);"
# drive each class: single INSERT, multi-row INSERT, COPY, UPDATE, DELETE
psql -c "SELECT * FROM heliosdb_write_volume;"
```

Read `version_bytes / (data_bytes + version_bytes)` on the `insert_single`,
`insert_multi`, and `copy` rows. `rows` gives per-row averages (`*_bytes / rows`).
Expected: `insert_single`/`insert_multi` version share ≳ 50%; `copy` version share
**near zero** (already elided to a single `vmeta:` marker — the win this design
generalizes). `update`/`delete` version share **zero** (fast paths are
version-skipping, §1.5).

### 1.4 Gate reading (filled at the coordinator gate)

> **REQUIRED BEFORE MERGE — this block is the STOP-rule input, not optional.** The
> 53–72% in §1.2 is an analytic *prior*; the go/no-go is decided off the measured
> numbers below. The coordinator MUST run the `heliosdb_write_volume` gate (§1.3) on
> a **time-travel-ON** build (default/`safe`/`balanced`/`agent` — §1.6) and fill this
> block before PROCEED is validated. It is left blank here because this session's host
> rules forbid live-server and benchmark runs.

```
# MEASURED 2026-07-17 (coordinator gate, chain B run bm5ek1l05):
# build=release @ c5c762a (default profile, time-travel ON), write_volume_stats=true
insert_single   data=43      version=80  index=12      rows=1      version_share=65%
insert_multi    (recorded 0 — landed in `other`: data=114 version=225 rows=3 -> 66%;
                 classification gap: the 3-row VALUES insert is not tagged
                 insert_multi. Filed with the lease; the share itself is consistent.)
copy            data=467,804 version=57  index=120,000 rows=10,000 version_share=~0%
                (v4.1.0 vmeta: range markers already collapse COPY version writes
                 to one marker — independent cross-validation of that design.)
update          data=40      version=0   index=0       rows=1      (expect 0 ✓ — §1.5:
                 fast UPDATE/DELETE are version-skipping, watermark-invalidating)
delete          data=8       version=0   index=0       rows=1      (expect 0 ✓)
# fast-profile cross-check (§1.6): insert_single version=0 ✓ (all classes 0).
# Anomaly: fast-profile COPY recorded rows=0 — the \copy under profile="fast"
# either failed silently or routes around the instrumented funnel; filed with
# the lease (does not affect the verdict, which needs the time-travel-ON run).
Verdict: PROCEED  because the measured version_share on single-row INSERT is
65% (and 66% on the 3-row batch via `other`) — above both the 15% deprioritize
floor and the 50% expectation from §1.2's analytic prior. The single-copy
latest-version design (flagged v_idx: event + materialize-on-first-mutation)
targets a majority of INSERT byte volume. COPY is already solved by vmeta:.
NO format change ships this campaign (binding stop rule); implementation goes
with the W3 lease.
```

### 1.5 A finding the census makes explicit

The autocommit fast UPDATE/DELETE paths (`update_tuple_fast*`, `delete_tuple_fast*`,
`engine.rs:11250-11460`) are already **version-skipping** — they overwrite/tombstone
`data:` with no `v:`/`v_idx:` write (the pre-existing "D4 gap", `copy_marker.rs:24-28`).
So the `v:` duplication is overwhelmingly an **INSERT-path** phenomenon on the default
autocommit path; a single-copy latest version targets exactly the class that pays it.
(The buffered/explicit-transaction path DOES version UPDATE/DELETE via
`put_version_index_batch`; those bytes land in the `other` census class, §2.3.)

### 1.6 Profile dependence — the win is a time-travel-ON phenomenon (scope caveat)

`version_bytes` is **zero whenever `time_travel_enabled = false`**: `insert_tuple_fast`
allocates a version timestamp and calls the version funnel ONLY under
`self.config.storage.time_travel_enabled` (`engine.rs:10520`; the
`write_data_version_and_register_snapshot`/`write_version_and_register_snapshot` calls at
`:10579-10611` run only when that timestamp is `Some`, so
`append_version_snapshot_to_batch` never fires otherwise). Two shipped profiles turn
time-travel OFF — `fast` (`config.rs:616`) and `fast_ingest` (`config.rs:628`) — and under
them NO `v:`/`v_idx:` is written, so a single-copy latest version saves **nothing**: their
INSERT byte volume is already version-free.

The optimization's addressable scope is therefore exactly the **time-travel-ON** configs:
the default (`config.rs:484`), `safe` (`:602`), `balanced` (`:609`), and — deliberately —
`agent` (`:640`, whose bundle keeps time-travel ON because AS OF reads and AS OF branch
anchors are part of the agentic surface, comment at `config.rs:633-637`). The `agent`
profile is a natural throughput user that IS in scope; only the pure bulk-ingest
`fast`/`fast_ingest` profiles opt out. The census confirms this empirically: run the gate
once under a time-travel-OFF profile and `version` must read zero for every INSERT class;
the PROCEED estimate (§1.2) applies only to the time-travel-ON census — the coordinator
should measure §1.4 on such a build.

---

## 2. Instrumentation delivered (this commit)

### 2.1 Mechanism — per-class byte census on the write funnels

Module `src/write_volume.rs`. A process-global `[StmtClass × Category]` matrix of relaxed
`AtomicU64`s counts durable bytes by statement class (`insert_single`, `insert_multi`,
`copy`, `update`, `delete`, `other`) and key family (`data`, `version`, `index_key`),
plus a per-class row-event count. Statement class rides a thread-local `Cell<StmtClass>`
set by a RAII [`stmt_scope`] guard at the DML boundary; storage writes run synchronously
on the dispatching thread (no `.await` between scope and RocksDB write), so the class is
exact for autocommit statements.

### 2.2 Zero cost when disabled — runtime-only, NO cargo feature

Gate: one relaxed load of a process-global `AtomicBool` (`[performance]
write_volume_stats`, default `false`), mirroring `global_txn_active` (`lib.rs:540`) and
the copy-marker `any` fast-out (`copy_marker.rs:53`). **The claim that this needs no
cargo feature is proved by counting the atoms per row** (module header,
`write_volume.rs:20-37`): DISABLED, the RAII `stmt_scope` guard pays one relaxed load on
construction (`stmt_scope` → `enabled()`, `write_volume.rs:195`) and each write funnel a
row crosses pays exactly **one** more (the funnel hoists `enabled()` once and gates all its
`add`/`add_row` calls on it), so:

- autocommit single INSERT = **4 loads** (scope guard + data funnel `insert_tuple_fast` +
  version funnel `append_version_snapshot_to_batch` + index funnel `on_insert_tuple`);
  **3 loads** under a time-travel-OFF profile (§1.6), where the version funnel never runs;
- autocommit UPDATE / DELETE = **2 loads** (scope guard + data funnel — version-skipping);
- COPY / multi-row INSERT = **1 scope load** + **1 load** hoisted before the batch loop +
  1 index load, per *statement* — the per-row cost inside the loop is zero (the gate is
  hoisted out of it).

Each is a predictable not-taken branch over a single `mov`; no store, fence, lock,
thread-local access, or allocation on the disabled path. Relaxed atomic *loads* of an
almost-always-false, cache-resident, uncontended global are free at steady state — which
is why a `#[cfg(feature)]` (as W3.1 `lock_census` needed for its try-lock sampling) would
be over-engineering here. ENABLED: one `Relaxed` `fetch_add` per recorded category plus
one `add_row` — a diagnostic aggregate, not a serialization point.

### 2.3 Instrumented funnels (coverage — read this before trusting a number)

| statement class | data: | version | index-key | funnel(s) | attribution |
|-----------------|-------|---------|-----------|-----------|-------------|
| `insert_single` | ✓ | ✓ | ✓ | `engine.rs insert_tuple_fast:10444/10491` → `time_travel.rs append_version_snapshot_to_batch:1087` → `art_manager.rs on_insert_tuple:1552` | exact (scope in `insert_tuple_fast`) |
| `insert_multi`  | ✓ | ✓ | ✓ | `engine.rs insert_prepared_tuples_fast_batch:10733-10843` (+ tail `on_insert_tuple`) or txn-commit branch `put_versioned_batch` | scope at `lib.rs try_fast_insert_many_params:6411` — direct-batch & txn-commit branches only; per-row logical-WAL fallback re-scopes to `insert_single` (note 4) |
| `copy`          | ✓ | ✓ (one `vmeta:` marker/batch) | ✓ | `insert_prepared_tuples_fast_batch` vrange branch | scope at `lib.rs copy_bulk_insert:6535` |
| `update`        | ✓ | n/a (version-skipping) | ✗ (via `on_update`, not counted) | `engine.rs update_tuple_fast*:11250/11339` | scope in the engine method |
| `delete`        | ✓ (tombstone key) | n/a | ✗ (via `on_delete_tuple`, not counted) | `engine.rs delete_tuple_fast*:11385/11436` | scope in the engine method |
| `other`         | ✓ | ✓ | ✓ (txn insert) | `transaction.rs put_versioned_batch:986` + `put_version_index_batch:1052` + `commit delete arm:802` | ambient (unscoped) |

**Documented partial coverage** (stated so a reader never over-trusts a bucket):
1. **Buffered/explicit-transaction writes land at COMMIT time**, decoupled from the
   staging statement, so their class is whatever scope is active at `COMMIT` — normally
   `other`. Only autocommit-implicit transactions (e.g. the multi-INSERT txn-commit
   branch `lib.rs:6461`, which runs inside `try_fast_insert_many_params`' scope) attribute
   to their true class. The **headline** — autocommit INSERT — is exact; explicit-txn
   bulk-load rolls into `other`.
2. **The non-fast versioned INSERT path** (`insert_tuple_versioned_with_schema` →
   `write_version`, used by `INSERT … SELECT` and shapes the fast spec rejects) is NOT
   instrumented — recording only its version there without its `data:` write would skew
   the ratio, so it is omitted entirely and its rows simply do not appear in the census.
3. **UPDATE/DELETE secondary-index maintenance** (`on_update`, `on_delete_tuple`,
   `remove_single_pk_key`) is not counted; index-key bytes are captured for INSERT classes
   only (the headline). Columnar `col:`/`colz:`/`colp:` sidecar bytes are out of scope
   (the default row-store is the measured case).
4. **A multi-row INSERT's per-row logical-WAL fallback attributes to `insert_single`, not
   `insert_multi`.** When `fast_dml_requires_logical_wal()` is true (HA / logical-WAL
   configs) `try_fast_insert_many_params` inserts row-by-row via
   `StorageEngine::insert_tuple_fast` (`lib.rs:6496`), which opens its OWN `InsertSingle`
   scope (`engine.rs:10444`) that shadows the ambient `InsertMulti` for each row — so under
   those configs a multi-row INSERT lands in the `insert_single` bucket. This does not move
   the go/no-go (§7): `insert_single` and `insert_multi` carry the same high version share,
   so for the STOP decision read the **sum** of the two buckets. The direct-batch and
   autocommit-txn-commit branches (`lib.rs:6439-6491`) keep the true `insert_multi` class.

### 2.4 Surface (interface-coverage gate #5)

- **Runtime knob** `[performance] write_volume_stats` (config.rs `PerformanceConfig`,
  default `false`; documented in `config.example.toml`). Applied at
  `EmbeddedDatabase::with_config` via `write_volume::set_enabled` (process-global; last
  config wins — a diagnostic aggregate).
- **System view** `heliosdb_write_volume` (registered in `sql/phase3/system_views.rs`,
  dispatched to `execute_heliosdb_write_volume`) — one row per statement class with
  columns `stmt_class, data_bytes, version_bytes, index_key_bytes, rows`. Reachable as
  `SELECT * FROM heliosdb_write_volume` (bare) and `heliosdb_write_volume()` (function
  form). Always six rows; zeros unless enabled.
- **REPL** `\stats` hint (`repl/commands.rs`).

No magic numbers: the census introduces one boolean knob and no thresholds; the 15% STOP
figure is a documentation constant in this file, not a runtime parameter.

---

## 3. Design: single-copy latest version (flagged `v_idx:` event + materialize-on-first-mutation)

**Status: IMPLEMENTED behind the default-OFF `[storage] elide_latest_version`
knob (W3 lease, branch `perf/w3-impl-2026-07-17`).** The on-disk format is
unchanged when the knob is off (the shipped default); turning it on is the
one-way, release-noted door of §4/§6. The mechanism is a generalization of
`copy_marker.rs` from contiguous COPY batches to every main-branch INSERT. See
§8 for the as-built flag encoding and the funnels touched.

### 3.1 The prior art (`copy_marker.rs`, v4.1.0)

A time-travel COPY fast-batch already elides per-row `v:`/`v_idx:` and instead writes ONE
`vmeta:{table}:{first}:{last} → commit_ts` range marker over the contiguous row-ids
(`engine.rs:10746`). Two questions the elided versions answered are re-answered by the
marker + `data:`:

1. **AS-OF read of a COPY'd, never-mutated row**: visible iff `snapshot_ts >= marker_ts`,
   value = current `data:` (`copy_marker.rs:16-19`, `covering_ts`).
2. **First mutation of such a row** materializes the insert version (`v:`/`v_idx:` at
   `marker_ts`) FROM the pre-image `data:` value into the SAME WriteBatch, BEFORE
   overwriting `data:` (`time_travel.rs materialize_copy_marker_row:987-1055`, called from
   the commit path `transaction.rs:774` and the fast UPDATE/DELETE paths via
   `materialize_copy_marker_row_durable`, `engine.rs:11209/11291/11331`).

The correctness envelope is already proven for COPY. W3.2 asks: apply it to single-row
and multi-row INSERTs, whose row-ids are NOT a contiguous range and so cannot share one
range marker.

### 3.2 The mechanism for general inserts

Keep the small `v_idx:` **event** per inserted row (the AS-OF discovery index — 8-byte
value, `v_idx:{table}:{row_id}:{020 reverse_ts} → commit_ts`); **elide only the `v:`
key+value** (the full row copy, §1.1). Encode a one-bit **flag** in the `v_idx:`
value meaning "the version value for this event is ELIDED — read it from `data:` (this is
the latest version) unless a later event materialized it."

- **Insert (new format, flag ON):** write `data:` + `v_idx:` event with the elided flag.
  Do NOT write `v:`. Net: 1 memtable put dropped and one payload-sized value copy removed
  per insert (the §1 win).
- **First mutation (UPDATE/DELETE) of a flagged row:** BEFORE overwriting `data:`,
  materialize the insert `v:` from the current `data:` value at the event's `commit_ts`
  and clear the flag (rewrite the `v_idx:` value without the flag bit) — all in the
  mutation's WriteBatch. Identical to `materialize_copy_marker_row`, but driven by the
  per-row flag instead of the range marker.
- **AS-OF read:** seek `v_idx:` `SeekForPrev(reverse_ts(snapshot_ts))` as today
  (`time_travel.rs:748`). If the found event's flag is CLEAR, read `v:` (present). If SET,
  the value is the current `data:` — valid because a SET flag means no later version
  materialized this event, i.e. the row was never mutated after this insert, so `data:`
  still holds the insert value.

### 3.3 Why the flag lives in the `v_idx:` value, not a separate probe

`copy_marker` answers "is this row still elided?" with `has_any_version_index` — an extra
RocksDB prefix scan (`time_travel.rs:958`). For a per-row design that would reintroduce
exactly the per-statement metadata probe W1.3 fought to remove. Encoding the flag IN the
existing 8-byte `v_idx:` value (e.g. a reserved high bit of the big-endian `commit_ts`, or
a 1-byte tag prefix) keeps the read path a single seek with no extra get, and keeps the
flag **durable** — no in-memory interval set to rebuild on open (simpler than
`copy_marker`'s `CopyMarkers`, which reloads from a `vmeta:` scan, `copy_marker.rs:113`).
The flag encoding MUST ship with a forever-fallback decoder (§4).

---

## 4. On-disk compatibility matrix

Format evolution reuses the **W2.2(b) precedent exactly**: `deserialize_snapshot_metadata`
(`time_travel.rs:96-123`) parses the current layout first and falls back to
`LegacySnapshotMetadata` (`:66-95`) forever — "NEVER remove: pre-W2.2 databases keep [the
legacy layout]". The `v_idx:`-flag decoder gets the same treatment.

| direction | behavior | correctness |
|-----------|----------|-------------|
| **Old data, new binary** (upgrade) | Old `v_idx:` values carry no flag bit; the new decoder treats "no flag" as **flag CLEAR = `v:` present** (the pre-W3.2 invariant). Every old insert wrote a full `v:`, so a clear flag + present `v:` reads exactly as before. | ✓ Safe. The DEFAULT interpretation of an unflagged value is the old behavior — the same "parse new, fall back to old" shape as W2.2(b). Never remove the clear-flag default. |
| **New data, new binary** | Flagged (elided) events read `data:` for the latest version; materialized events read `v:`. Mixed old+new rows in one dir resolve per-row by their own flag — the transition is **per-row, not global** (a table can hold pre-upgrade full-`v:` rows and post-upgrade elided rows simultaneously). | ✓ Mirrors `copy_marker`'s "mixed old+new format rows" AS-OF test envelope (WAVE spec W2.2 test line). |
| **New data, OLD binary** (downgrade) | **OUT OF SCOPE — documented failure mode.** An old binary seeks `v:{table}:{row}:{ts}` for a flagged (elided) insert version, finds nothing, and resolves the `AS OF` read at that `ts` as "row did not exist" — a wrong historical answer. It cannot fall back to `data:` because it does not know the flag. | ✗ One-way door. **Latest-version reads (via `data:`) are unaffected** — only historical `AS OF` at the insert ts of a never-mutated, post-upgrade row mis-resolves. |

**Downgrade mitigation (design requirement):** the new format is gated behind a config
flag (§6) that defaults OFF; enabling it is a one-way, release-noted decision, exactly as
W2.2(b) treated the epoch-micros switch. A dump/restore (`heliosdb-nano dump`) re-serializes
through the logical row values and is format-agnostic, providing a downgrade escape hatch.

---

## 5. Interaction analysis (the load-bearing correctness sections)

### 5.1 AS OF query correctness across the transition

The version-resolution entry point is `read_at_snapshot` /
`read_at_snapshot_uncached` (`time_travel.rs:747`), which today (post-`copy_marker`)
already handles a row with NO `v_idx:` by consulting the COPY marker (`:799`). The flagged
design ADDS a third case to the existing two:

1. `v_idx:` event found, flag CLEAR → read `v:` (unchanged from today).
2. `v_idx:` event found, flag SET, `snapshot_ts >= event_ts` → value is `data:` (new).
3. no `v_idx:` event at all → COPY marker path (unchanged, `copy_marker` envelope).

Because resolution keys off each row's own durable flag, a query spanning old-format
rows (case 1), new-format never-mutated rows (case 2), new-format materialized rows
(case 1 after materialization), and COPY rows (case 3) is correct row-by-row with no
global migration barrier. The snapshot-GC (`version_gc.rs`) must learn the flag: it must
NOT reclaim a flagged event's `data:`-backed value (there is no `v:` to reclaim, and the
`data:` row is the live row) — GC already scans `v:`/`v_idx:` only (`version_gc.rs:527`),
so a flagged event with no `v:` is naturally skipped; the design must assert this in the
GC test matrix.

### 5.2 Branch overlay (`bdata:`) interaction

Branch writes live under `bdata:{branch}:…` and never enter `written_tables`
(`transaction.rs:145-153`); `copy_marker` is likewise main-branch-only. **The design
scopes elision to main-branch `data:` only.** Branch inserts (`insert_tuple_branch_aware`,
`engine.rs:12716`) keep whatever versioning the branch path uses today — branches are
ephemeral fork-test-discard sandboxes (per the branches skill), so their write volume is
not the target and adding a branch-scoped flag/materialize is unjustified complexity.
Requirement: the elision flag is written ONLY on the main-branch insert funnels; a branch
switch (`set_current_branch`, `engine.rs:12422`) does not change how already-written main
rows resolve (their flag is durable and per-row). A merge-to-main of branch rows
(`engine.rs:8960/9034`) must route through the main insert funnel so merged rows get the
main-branch treatment (flagged or full-`v:` per the config at merge time).

### 5.3 WAL-streaming replication, mixed-version primary/standby

Nano replicates **logical** operations, not physical RocksDB bytes: an insert is logged
via `log_data_insert` and the standby REPLAYS it through its own insert path. Therefore
**each node applies its own on-disk format independently**:

- new primary + old standby: the standby replays the logical insert and writes a full
  `v:` (old format) — correct, no elision, no flag needed on the wire;
- old primary + new standby: the standby replays and elides — correct.

So logical streaming replication is **format-agnostic and mixed-version-safe** for this
change (the wire carries the row, not the `v:`/`v_idx:` bytes). The one hazard is if a
future physical/SST-shipping replication mode is added: it would ship the flagged `v_idx:`
bytes raw, and an old standby could not interpret the flag — so **physical replication of
the new format requires standby ≥ the format-introducing version** (the same one-way
constraint as §4 downgrade). This is a forward requirement to record now; no format ships
this campaign, so there is nothing to gate yet. Cite the logical-WAL insert path
(`engine.rs log_data_insert`) and the replication apply path when implementing.

### 5.4 Crash-recovery windows

Atomicity is by WriteBatch construction, exactly as `copy_marker`:

- **Crash after insert, before any mutation:** `data:` + flagged `v_idx:` are in one batch
  (or the fast path's `write_data_version_and_register_snapshot` single batch,
  `time_travel.rs:890`). Recovery sees a never-mutated flagged row → AS-OF resolves via
  `data:` (§5.1 case 2). Correct.
- **Crash during first mutation:** the materialize-insert-`v:` put and the new `data:`/`v:`
  puts ride the SAME batch (the `materialize_copy_marker_row` precedent stages into the
  mutation's batch, `transaction.rs:774`). RocksDB applies all-or-nothing → either the
  pre-materialization state (flagged, value in `data:`) or the post state (flag cleared,
  insert `v:` materialized, new version written). Both are consistent.
- **In-memory state:** unlike `copy_marker` (which rebuilds a `CopyMarkers` interval set
  from a `vmeta:` scan on open, `copy_marker.rs:113`), the per-row flag is DURABLE in the
  `v_idx:` value, so recovery is **stateless** w.r.t. elision — no scan-on-open, no
  reconstruction. This is a simplification the general design buys over the range-marker
  approach.

---

## 6. Interface coverage & tunables for the FUTURE format (gate #5)

The instrumentation's knob (`[performance] write_volume_stats`) is the only one shipping
now. The future format, when/if implemented, MUST be a config toggle (no silent format
switch), proposed as:

- `[storage] elide_latest_version = false` (default OFF — a one-way, release-noted door,
  §4). ON makes new inserts write the flagged elided form; OFF keeps full `v:`. Both
  formats remain readable forever (§4). A per-value flag means the toggle can flip without
  a migration — old flagged rows stay flagged, new rows follow the current setting.

No hardcoded threshold is introduced; the 15% STOP figure lives in §1 as documentation.

---

## 7. Go / No-Go (coordinator decision)

**PROCEED to a future implementation task** iff §1.4 shows `insert_single`/`insert_multi`
`version_bytes / (data_bytes + version_bytes)` ≥ 15% (estimate: 53–72%; read the **sum** of
the two INSERT buckets — §2.3 note 4 — since a multi-row INSERT's per-row logical-WAL
fallback attributes to `insert_single`). Then the implementation is a SEPARATE, gated task
(not this campaign) that ships §3 behind the §6 config flag with the §4/§5 matrices as its
test plan, and re-runs `heliosdb_write_volume` to confirm the `version` bucket collapses for
`insert_single`/`insert_multi`.

**Scope caveat (§1.6) — the benefit is time-travel-ON only.** Version duplication is a
`default`/`safe`/`balanced`/`agent` phenomenon: those profiles keep `time_travel_enabled =
true`, so every versioned INSERT pays the `v:` copy. The `fast` and `fast_ingest` profiles
disable time-travel (`config.rs:616`, `:628`) and write **zero** version bytes — a workload
pinned to those profiles gains nothing from a single-copy latest version, and the §1.4
census will read `version = 0` for every INSERT class under them. The `agent` profile, by
contrast, deliberately keeps time-travel ON (`config.rs:640`) and IS in scope. Weigh the
estimate against the target deployment's profile before treating PROCEED as universal, and
measure §1.4 on a time-travel-ON build.

**DEPRIORITIZE and record here** iff the census contradicts the estimate and the share is
<15% (would require rows so wide that the payload dwarfs the doubled payload — arithmetically
impossible, since `version_share > V/(2V) = 50%` whenever keys are non-negative and the
`v:` value equals the `data:` value; a <15% reading therefore indicates the workload is
UPDATE/DELETE-dominated, not INSERT — in which case the single-copy latest version is
correctly deprioritized because there is little INSERT volume to save).

---

## 8. Implementation notes (as built, W3 lease)

Landed behind `[storage] elide_latest_version = false` (default OFF;
`config.rs` `StorageConfig` + `config.example.toml`). Wired at
`SnapshotManager::configure_elision`, called from both `StorageEngine` open
paths (durable + in-memory) after `recover_snapshots`.

**Flag encoding (§3.3 decision).** The elision flag is the reserved **high bit
of the big-endian `commit_ts`** stored in the existing 8-byte `v_idx:` event
value (`VERSION_VALUE_ELIDED_FLAG = 1 << 63`, `storage/time_travel.rs`).
`commit_ts` is a logical/epoch-micros counter far below 2^62, so bit 63 is free
for ~292k years and can never collide with a real timestamp. Chosen over a
1-byte tag prefix because it keeps the value exactly 8 bytes (no read-path length
change) and needs no separate probe. **Forever-fallback:** `decode_version_index_value`
treats any value with the high bit clear — which is *every* pre-W3.2 value — as
flag CLEAR = `v:` present (§4 row 1). `encode_version_index_value(ts, false)` is
byte-identical to the legacy `ts.to_be_bytes()`, so an off-knob build is on-disk
identical to pre-W3.2. NEVER remove the clear-flag default.

**Funnels touched (§5.2 decision), main-branch only.**
- *Elide* (write flagged event, skip `v:`): the fast single-INSERT version funnel
  `append_version_snapshot_to_batch` (via `write_data_version_and_register_snapshot`
  / `write_version_and_register_snapshot`, gated `allow_elide = !uses_side_storage`
  — side-storage keeps full `v:` because `data:` there holds the sidecar image,
  not the version value); and the buffered/txn-commit writers
  `transaction.rs put_versioned_batch` / `put_version_index_batch`.
  `insert_prepared_tuples_fast_batch` is deliberately NOT changed — its COPY
  `vmeta:` range-marker path already elides, and its per-row branch (columnar /
  vrange-off) keeps full `v:`.
- *Materialize-before-overwrite* (write the real `v:` from `data:`, clear the
  flag, in the mutation's WriteBatch): folded into the existing copy-marker
  materialize sites so all mutation funnels are covered at once —
  `materialize_copy_marker_row_durable` (the 7 fast/generic UPDATE/DELETE + bulk
  sites) now also runs `materialize_elided_latest_version_durable`, and the
  transaction commit loop calls `materialize_elided_latest_version` alongside
  `materialize_copy_marker_row`. TRUNCATE's `materialize_copy_markers_for_table`
  gate now also admits the scan when elided rows may exist. **ALTER TABLE
  ADD/DROP COLUMN** (`add_column_to_rows` / `drop_column_from_rows`) rewrite every
  main `data:` row in place with no new version — an otherwise-uncovered mutation
  funnel — so they likewise call `materialize_copy_marker_row_durable` before each
  row's overwrite, gated on `table_has_copy_markers` / `maybe_elided_rows` hoisted
  out of the rewrite loop. Without this, a pre-ALTER AS-OF read of an elided row
  would resolve its flagged event to the *rewritten* `data:` and W3.5's
  `null_pad`/truncate would project the added/dropped column from that wrong
  pre-image instead of the insert-time value; with it, the AS-OF read resolves the
  materialized insert `v:` and W3.5 shapes the correct pre-image under the new
  arity (§5.1 parity with the elision-OFF path).
- *Read* (§5.1 case 2): `read_at_snapshot_uncached` decodes the flag; flag SET +
  `snapshot_ts >= event_ts` ⇒ value is current `data:`; flag CLEAR ⇒ `v:`
  (unchanged); no `v_idx:` ⇒ COPY-marker path (unchanged).

**Materialize gate (durability / statelessness).** A durable sentinel key
`w3_2_elide_used` is written once when elision is enabled and loaded on open into
`maybe_elided_rows`; the mutation hot path pays one relaxed atomic load and skips
the per-row `v_idx:` seek entirely when no flagged row can exist. This keeps
recovery stateless (a point-get, not a scan) AND keeps a reopen with the knob
toggled OFF correct: rows a prior session left flagged are still materialized on
their next overwrite (old flagged rows stay flagged — no migration, §6).

**GC (§5.1).** `version_gc` scans the `v:` keyspace only, so a flagged event with
no `v:` never enters a collection group and its `data:`-backed value is never
reclaimed while the row lives — asserted by the census/GC tests.

**Tests.** Core mechanism (encode/decode + forever-fallback, elided-insert omits
`v:`, materialize-on-first-overwrite preserves AS-OF, mixed per-row resolution,
legacy-value fallback, sentinel re-arm across reopen) in `storage/time_travel.rs`
unit tests; end-to-end SQL (version-key collapse vs the OFF control, AS-OF across
insert→versioned-update and insert→fast-update, VACUUM skips elided rows, mixed
coexistence) in `tests/w3_2_elide_latest_version_tests.rs`.

**Deferred / recorded risks.** (a) The live `heliosdb_write_volume` §7
re-measurement (`insert_single`/`insert_multi` version bucket collapse) is a
coordinator gate — a unit test cannot enable the process-global census without
racing the shared test binary, so it is left as the SQL-level `version_keys`
collapse assertion plus the live re-run. (b) Logical streaming replication is
format-agnostic (§5.3: the standby replays the logical insert through its own
funnel and applies its own on-disk format), so no wire change is needed; a
dedicated mixed-version HA replay test needs the multi-node HA harness and is not
added here. (c) `merge_branch` into main still raw-`put`s branch rows into
`data:` with NO version and materializes nothing (pre-existing, version-blind
behavior — it does not call `materialize_copy_marker_row_durable` either).
Consequences, unchanged by W3.2: a merged NEW row carries no `v_idx:` and
resolves via the no-version → `data:` read path (case 3); a merge that OVERWRITES
an existing main row whose latest version is elided loses that AS-OF value at the
overwrite — but this is the SAME pre-existing limitation copy-marker rows already
have under merge (merge preserves neither), so elided rows are no worse than the
v4.1.0 baseline. Merge is a discouraged/unreliable path (the branches skill says
"prefer discarding and re-applying validated SQL to main"). Routing merge through
the insert funnel (design §5.2 aspiration) would fix the pre-existing
version-blindness for both marker and elided rows and is a separate, larger
change — NOT taken here to keep W3.2 contained.
