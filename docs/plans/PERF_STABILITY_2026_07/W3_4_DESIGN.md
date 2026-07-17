# W3.4 — ART maintenance share of COPY: measurement + (conditional) batching design

Status: **DESIGN-FIRST. Instrumentation landed; the go/no-go is a coordinator gate
decision read off the counters, not an assumption. NO engine behavior change ships
this campaign.** Base: `perf/w3-design` off v4.2.0 (`a2e1b5b`). Companion analysis:
`PERF_ANALYSIS_2026_07_13.md` §"WAVE 3" (W3.4), spec `WAVE_IMPL_SPEC_2026_07_16.md`
§W3.4. Sibling instrumentation precedent this reuses: `src/write_volume.rs` (W3.2,
per-class byte census) and `src/lock_census.rs` (W3.1).

> **STOP rule (binding).** ART-index maintenance is batched (§3) ONLY if it is
> **≥8% of COPY wall time**. §1.4 is the measured input; the coordinator fills it
> from `heliosdb_copy_phase_stats` after the §1.3 runs. If ART maintenance is
> **<8%**, record the top cost in §1.4, name the item that should replace this one
> in §5, and STOP — no batching work. The 8% figure is the roadmap's documentation
> constant, not a runtime parameter.

---

## 1. Measurement (the STOP-rule input)

### 1.1 The funnel and its phases

The single COPY funnel is `copy_bulk_insert` (`lib.rs:6542`), the W2.1 fast path,
which after decode calls `StorageEngine::insert_prepared_tuples_fast_batch`
(`engine.rs:10632`) for the one atomic WriteBatch. The instrumentation splits its
wall time across ten phases:

| phase (view row) | region | file:line |
|------------------|--------|-----------|
| `decode`         | wire text/CSV frame decode (disjoint from `total`) | `handler.rs handle_copy` `CopyData` arm + `decoder.finish()` |
| `type_convert`   | `String` → typed `Value` per row (`materialize_copy_tuple`) | `lib.rs:6601-6610` |
| `check_constraint` | CHECK evaluation (`validate_check_constraints`) | `lib.rs:6615-6624` |
| `prepare`        | PK/SERIAL auto-fill (`prepare_fast_insert_batch`) | `lib.rs:6632-6638` |
| `validate_batch` | NOT NULL + duplicate-PK (`validate_fast_insert_batch`) | `lib.rs:6639-6644` |
| `fk_constraint`  | FK probes (`validate_copy_batch_fks`) | `lib.rs:6650-6657` |
| `batch_build`    | serialize + `data:`/`v:`/`vmeta:`/columnar WriteBatch build | `engine.rs:10702-10884` |
| `commit`         | `db.write(batch)` durable RocksDB write | `engine.rs:10886-10894` |
| `art_maintain`   | `on_insert_tuple` + HNSW `on_row_insert` per row | `engine.rs:10909` loop |
| `total`          | whole `copy_bulk_insert` insert work (denominator) | `lib.rs:6597` |

`total` wraps the engine-side funnel; `decode` runs on the wire before it (disjoint),
so full server-side COPY wall time is `total + decode`. Phases `type_convert` ..
`art_maintain` sum to `total` minus a small un-attributed remainder (shape bails, spec
resolve, SMFI guard, `durable_autocommit_barrier`, snapshot register). The ART-maintenance
walk is `on_insert_tuple` (`art_manager.rs:1548`), whose per-row cost is the subject of §3.

### 1.2 First-principles prior (why the outcome is genuinely unknown)

Unlike W3.2 (where `version_share > 50%` is arithmetically forced), the ART share has
no closed form — it depends on the number of secondary indexes on the target table and
on the total number of registered indexes system-wide (`on_insert_tuple` walks the
GLOBAL index map, §3.1). Two regimes bracket it:

- **Unconstrained/low-index COPY** (bench `t100000(aid int, abalance int)`, no PK, no
  index): `on_insert_tuple` iterates the global map and matches **zero** entries for
  this table → `art_maintain` is a near-empty loop. Expected **well below 8%**; the cost
  is dominated by `batch_build` (serialize + `data:` puts) and `commit` (the one durable
  RocksDB write of ~100k `data:` keys). This is the pure STOP candidate.
- **Constrained COPY** (Pagila-like: a PK ART + 2 FK ARTs, §1.3 recipe): three ART
  inserts per row, each an encode + tree write, PLUS the global-map scan overhead. This
  is where ART share could cross 8% — and where the §3 design pays off.

The gate decides which regime the roadmap's target COPY sits in. The instrumentation is
built to attribute it exactly; the estimate here is a prior, not the verdict.

### 1.3 How the gate runs it (coordinator, single heavy-op slot)

The census is process-global and cumulative; **use a FRESH nano process per workload**
(the statics start zeroed) and read the view once after a single COPY. Enable via a
config file (`copy_phase_stats` has no CLI flag). Bound every load-generating step per
the host rules.

```bash
# 0. build (coordinator owns the heavy-op slot)
flock /home/gpc/HDB/sprint/coordination/build.lock \
  systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0 -- \
  cargo build --release

cat > /tmp/w34.toml <<'EOF'
# default profile keeps time_travel on (the dominant real-world COPY); the
# COPY fast path elides per-row v:/v_idx: to one vmeta: marker regardless.
[performance]
copy_phase_stats = true
EOF

BIN=./target/release/heliosdb-nano
# 100k-row CSVs, reusing the bench-engines.sh generator shape
awk 'BEGIN{for(i=1;i<=100000;i++)printf "%d,%d\n",i,(i*7)%100000}' > /tmp/ba2.csv        # unconstrained (2 col)
awk 'BEGIN{for(i=1;i<=100000;i++)printf "%d,%d,%d\n",i,((i*7)%100000)+1,((i*13)%100000)+1}' > /tmp/ba3.csv  # child (3 col)

# --- WORKLOAD A: unconstrained COPY 100k (bench-engines.sh storage cell) ---
D=$(mktemp -d); systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0 -- \
  "$BIN" start --listen 127.0.0.1 --port 5599 --auth trust --http-port 0 \
  --data-dir "$D" --config /tmp/w34.toml &
# wait for ready, then:
psql -h 127.0.0.1 -p 5599 -U postgres -c "CREATE TABLE big(aid int, abalance int);"
psql -h 127.0.0.1 -p 5599 -U postgres -c "\copy big FROM /tmp/ba2.csv WITH (FORMAT csv)"
psql -h 127.0.0.1 -p 5599 -U postgres -c "SELECT * FROM heliosdb_copy_phase_stats;"
# stop this nano; new process for B.

# --- WORKLOAD B: FK-COPY 100k (the W2.1 gate shape — 2-FK child) ---
D=$(mktemp -d); systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0 -- \
  "$BIN" start --listen 127.0.0.1 --port 5599 --auth trust --http-port 0 \
  --data-dir "$D" --config /tmp/w34.toml &
psql -h 127.0.0.1 -p 5599 -U postgres <<'SQL'
CREATE TABLE p (id int PRIMARY KEY);
CREATE TABLE q (id int PRIMARY KEY);
INSERT INTO p SELECT g FROM generate_series(1,100000) g;
INSERT INTO q SELECT g FROM generate_series(1,100000) g;
CREATE TABLE c (aid int, f1 int REFERENCES p(id), f2 int REFERENCES q(id));
SQL
psql -h 127.0.0.1 -p 5599 -U postgres -c "\copy c FROM /tmp/ba3.csv WITH (FORMAT csv)"
psql -h 127.0.0.1 -p 5599 -U postgres -c "SELECT * FROM heliosdb_copy_phase_stats;"
```

Read `total_nanos` per phase. **ART share = `art_maintain / (total + decode)`**; each
phase's own share = `phase / (total + decode)`. `total.rows` is the COPY row count N
(use it for per-row ns; `decode.rows` reads 0 by design — the count is not known at
push time). `unattributed = total − Σ(type_convert..art_maintain)`.

### 1.4 Gate reading (filled at the coordinator gate — REQUIRED before the STOP/GO edit)

> **This block is the STOP-rule input, not optional.** It is blank here because this
> session's host rules forbid live-server and benchmark runs. The coordinator MUST fill
> both workloads and set the verdict; the §5 STOP/GO is then a one-line edit.

```
# MEASURED 2026-07-17 (coordinator gate, chain B run bm5ek1l05; build=release
# @ c5c762a, copy_phase_stats=true, fresh process per workload)

# WORKLOAD A — unconstrained COPY 100k
decode           total_nanos=35,688,832   calls=145  rows=0
type_convert     total_nanos=34,501,153   calls=1    rows=100,000
check_constraint total_nanos=0            calls=0    rows=0
prepare          total_nanos=11,574,864   calls=1    rows=100,000
validate_batch   total_nanos=24,631,353   calls=1    rows=100,000
fk_constraint    total_nanos=0            calls=0    rows=0
batch_build      total_nanos=22,283,721   calls=1    rows=100,000
commit           total_nanos=28,051,491   calls=1    rows=100,000
art_maintain     total_nanos=30,037,754   calls=1    rows=100,000
total            total_nanos=153,878,141  calls=1    rows=100,000
  ART share = 30.04M / (153.88M + 35.69M) = 15.8%      top cost = decode (18.8%),
  then type_convert (18.2%), art_maintain (15.8%), commit (14.8%) — flat profile,
  no single dominant phase on the unconstrained path.

# WORKLOAD B — FK+CHECK COPY 100k (cat/item, 1 FK + 1 CHECK; the W2.1 fast path)
decode           total_nanos=45,445,064   calls=165  rows=0
type_convert     total_nanos=46,967,021   calls=2    rows=100,200
check_constraint total_nanos=88,524,956   calls=1    rows=100,000
prepare          total_nanos=9,964,778    calls=2    rows=100,200
validate_batch   total_nanos=20,582,623   calls=2    rows=100,200
fk_constraint    total_nanos=7,949,471    calls=1    rows=100,000
batch_build      total_nanos=19,627,337   calls=2    rows=100,200
commit           total_nanos=28,260,243   calls=2    rows=100,200
art_maintain     total_nanos=27,065,979   calls=2    rows=100,200
total            total_nanos=251,447,757  calls=2    rows=100,200
  ART share = 27.07M / (251.45M + 45.45M) = 9.1%       top cost = check_constraint
  (29.8% incl. decode in the denominator; 35.2% of `total`) — the per-row
  slow-path CHECK evaluator from W2.1 is now the single largest constrained-COPY
  cost. The batched ART FK probes themselves are cheap (2.7%).

Verdict: GO  because BOTH workloads clear the 8% floor (15.8% unconstrained,
9.1% constrained) — per-table entry lists + encode-once (§3) proceed with the
W3 implementation lease. ADDITIONAL FINDING for §5: batch-evaluate or compile
the CHECK expression (88.5ms for one trivial `qty >= 0` over 100k rows is
~885ns/row through the generic evaluator) — on constrained tables that is a
bigger prize than the ART batching this item was scoped for; file it as the
follow-on item alongside the ART work.
```

---

## 2. Instrumentation delivered (this commit)

### 2.1 Mechanism — RAII monotonic phase timers into relaxed atomics

Module `src/copy_phase_stats.rs`. A process-global `[Phase]` array of relaxed
`AtomicU64` triples (`nanos`, `calls`, `rows`). Each phase boundary is a RAII
[`PhaseTimer`] created by `copy_phase_stats::time(phase, rows)`: on drop it adds
`start.elapsed().as_nanos()` (monotonic `std::time::Instant`) to the phase's `nanos`,
one to `calls`, and the row count to `rows`. The guards wrap whole loops / calls, never
a per-row body, so a 100k-row COPY records each phase exactly once.

### 2.2 Zero cost when disabled — runtime-only, NO cargo feature

Gate: one relaxed load of a process-global `AtomicBool` (`[performance]
copy_phase_stats`, default `false`), mirroring `write_volume` (W3.2) and the
`global_txn_active` fast-out (`lib.rs:540`). When disabled, `time()` reads no clock and
returns an inert guard (`start: None`) — one predictable not-taken branch per phase
boundary, no `Instant::now`, no store, no allocation. There are **ten** boundaries per
COPY, all outside the per-row loops, so a disabled 100k-row COPY pays ten relaxed loads
total. This is why a `#[cfg(feature)]` (which `lock_census` needs for its per-acquisition
try-lock sampling) is over-engineering here — the same argument `write_volume.rs:12-44`
makes. ENABLED: one `Instant::now` per boundary + three `Relaxed` `fetch_add`s on drop —
a diagnostic aggregate, not a serialization point.

### 2.3 Attribution caveat (read before trusting a bucket)

`batch_build` / `commit` / `art_maintain` live in `insert_prepared_tuples_fast_batch`
(`engine.rs`), which is **shared with multi-row `INSERT ... VALUES`**
(`try_fast_insert_many_params`). The census is process-global, so read the view after a
**COPY-only** workload — exactly as `write_volume` is driven per statement class (§1.3
uses a fresh process per workload). The pre-write phases (`type_convert` .. `fk_constraint`,
`total`) are in `copy_bulk_insert`, which is COPY-exclusive, so they are always
COPY-exact. `decode` is wired only in the PostgreSQL wire handler; a COPY driven through
the embedded API or MySQL wire records `decode = 0` and the denominator is then `total`
alone (state which path the gate used).

### 2.4 Surface (interface-coverage gate #5)

- **Runtime knob** `[performance] copy_phase_stats` (`config.rs` `PerformanceConfig`,
  default `false`; documented in `config.example.toml`). Applied at
  `EmbeddedDatabase::with_config` via `copy_phase_stats::set_enabled` (`lib.rs`;
  process-global, last config wins).
- **System view** `heliosdb_copy_phase_stats` (registered in
  `sql/phase3/system_views.rs`, dispatched to `execute_heliosdb_copy_phase_stats`) — one
  row per phase with columns `phase, total_nanos, calls, rows`. Reachable as
  `SELECT * FROM heliosdb_copy_phase_stats` (bare) and `heliosdb_copy_phase_stats()`
  (function form). Always ten rows; zeros unless enabled.
- **REPL** `\stats` hint (`repl/commands.rs`).

No magic numbers: the census introduces one boolean knob and no thresholds; the 8% STOP
figure is a documentation constant in this file.

---

## 3. Design: per-table entry lists + encode-once (ships ONLY on a GO)

**This is a design. No engine change ships this campaign.** It is pursued only if §1.4
shows ART maintenance ≥8% of COPY wall time.

### 3.1 Verified current shape — `on_insert_tuple` is O(all registered indexes)

`on_insert_tuple` (`art_manager.rs:1548`) resolves a table's indexes by **iterating the
entire global index map and filtering by table**:

```rust
let indexes = self.indexes.read()…;              // art_manager.rs:1553
for entry in indexes.values() {                  // :1555  — ALL indexes, ALL tables
    if entry.table != table { continue; }        // :1556  — filter
    …encode key…; entry.tree.write()…insert…      // :1560-1579
}
```

So a COPY of N rows into a table with `k_own` indexes, while the process has `K_total`
registered indexes system-wide, pays **O(N · K_total)** map-scan work even when
`k_own ≪ K_total` — the "many-table scaling cliff" the roadmap names (write/bulk #11).
`on_insert_tuple_collect_index_values` (`:1588`), `on_update` (grep), and `on_delete`
(`:1649`) share the same full-map-scan shape.

**Why it is this shape today (verified, not assumed):** the manager DOES keep per-table
name maps — `pk_indexes` (`:194`), `fk_indexes` (`:196`), `unique_indexes` (`:200`) —
but the in-code contract at `art_manager.rs:185-188` states those maps **"do not cover
Manual (plain secondary) indexes, so iteration loops filter the entry map by
`entry.table` instead of relying on those maps."** There is no complete per-table →
all-index-names map, so the mutation loops cannot use the partial maps and fall back to
the global scan. That gap is exactly what §3.2 closes.

### 3.2 Per-table entry list (O(own indexes))

Add one complete index: `table_indexes: RwLock<HashMap<String, Vec<String>>>` mapping a
table to **all** its index names (PK, FK, Unique, AND Manual). Maintain it at the same
choke points that mutate `indexes`:

- **register** (`register_index`/`create_*_index` — grep the `indexes.write()` insert
  sites): push the new name onto `table_indexes[table]`.
- **drop** (`drop_table_indexes` `:704`, single-index drop): remove the name(s).
- **rename** (`rename_table_indexes` `:726`): move the vec to the new table key.
- **clear** (`clear_table_indexes` `:1839`): TRUNCATE keeps entries (trees cleared in
  place) — do NOT touch `table_indexes`.

Then the mutation loops resolve `table_indexes.get(table)` and iterate only those
entries — **O(k_own)** per row, independent of `K_total`. The `indexes` map stays the
source of truth for each `IndexEntry` (looked up by name); `table_indexes` is a derived
name index, invalidated at the same three DDL points, so it cannot drift (assert this in
the test matrix: register/drop/rename/clear each leave `table_indexes` consistent with a
full `indexes` filter).

Concurrency: `table_indexes` is read under the existing global-map read lock ordering
(read `table_indexes`, then per-`IndexEntry` tree writes one at a time — the same lock
discipline as today, `art_manager.rs:175-188`). No new lock rank.

### 3.3 Encode-once (serialize the key bytes once per row, reuse across the table's indexes)

Today each matched entry independently calls `index_value_refs_from_tuple` +
`encode_key_from_values` (`art_manager.rs:1560-1561`) — the per-column value encoding is
redone for every index, even when two indexes of the same table share a leading column.
On a GO, hoist the per-column byte encoding to once per row: encode each of the row's
indexed column values ONCE into a small scratch buffer keyed by column, then each index's
key is the concatenation of its columns' already-encoded fragments. This removes the
repeated `encode_value_into` work for overlapping index column sets (PK + a Manual index
on the same id, FK + covering index, …). The win compounds with §3.2 (fewer entries
walked AND each key built from cached fragments). Correctness envelope: the encoded bytes
must be byte-identical to today's `encode_key_from_values` output (the ART key format is
on-disk-durable via snapshots, `index_type_tag` `:231`) — the design REUSES the fragments,
never changes the encoding. A differential test (build a key via the current path vs the
encode-once path for every `DataType`) is the gate.

### 3.4 Batched tree locking (secondary, evaluate at implementation)

For a COPY, `on_insert_tuple` is called once per row, each taking+dropping the target
tree's write lock (`entry.tree.write()` `:1566`). A batched variant would take each own
index's tree lock ONCE for the whole batch and insert all N rows under it — amortizing
lock acquisition and improving ART node cache locality. This is the largest structural
change and the least certain; scope it only if §1.4 shows `art_maintain` dominated by
per-row lock/insert overhead rather than the map scan (which §3.2 already removes). Keep
the single-WriteBatch atomicity of the `data:`/`v:` write unchanged — ART maintenance is
in-memory and already runs after the durable commit (`engine.rs:10909`), so batching it
touches no durability contract.

---

## 4. Interface coverage & tunables (gate #5)

The instrumentation knob `[performance] copy_phase_stats` is the only surface shipping
now (§2.4). The §3 design adds **no** new knob: per-table entry lists and encode-once are
pure correctness-preserving structural changes with natural bounds (per-table index
counts), not tunables — there is no threshold to expose. If §3.4 batched tree locking is
pursued and needs a batch-size bound, it MUST reuse the existing COPY batch (already
bounded by `copy_max_buffered_rows`, `config.example.toml`), never a new constant.

---

## 5. Go / No-Go (coordinator decision)

**STOP — record and do no batching work — iff** §1.4 shows `art_maintain / (total +
decode)` **< 8%** on BOTH workloads. Then this item is closed by the measurement; fill
the top cost below and name its replacement:

```
STOP record (fill from §1.4):
  ART share:   unconstrained = ____%   FK = ____%   (both < 8%)
  Top COPY cost is: ______________  (expected: batch_build / commit — the data: puts +
                                     the single durable RocksDB write of ~100k keys)
  Replacement item: ____________________________________________________________
     (if `commit` dominates → durability/WAL-batching or write_opt tuning, not ART;
      if `batch_build` dominates → serialize/`data:`-key construction, cf. W3.2's
      single-copy latest version which removes the `v:` copy;
      if `decode` dominates → wire/CSV decode, a W2.4 follow-on.)
```

**GO — implement §3 as a SEPARATE, gated task (not this campaign) — iff** either
workload's ART share is **≥8%**. The implementation ships §3.2 (per-table entry list,
the O(own indexes) win — verified-needed because the partial per-table maps omit Manual
indexes, §3.1) and §3.3 (encode-once) behind the §3 test matrix (register/drop/rename/
clear consistency + a per-`DataType` encode differential), then re-runs
`heliosdb_copy_phase_stats` to confirm `art_maintain` share drops. §3.4 (batched tree
locking) is pursued only if the counters attribute `art_maintain` to per-row lock/insert
overhead after §3.2 removes the map scan.
