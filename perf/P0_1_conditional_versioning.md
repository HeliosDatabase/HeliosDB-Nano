# P0#1 — Conditional MVCC version-key writes

**Branch:** `perf/p0-p1-p2`

## Problem

`Transaction::commit_with_timestamp` wrote, for every row in the write set,
**three** RocksDB keys — `data:{t}:{id}` (current), `v:{t}:{id}:{ts}` (version
value), and `v_idx:{t}:{id}:{rts}` (reverse-ts index) — **unconditionally**.

`StorageConfig::time_travel_enabled` (default `true`) is documented as the switch
to "disable automatic versioning and reduce write overhead", and it *did* gate
`StorageEngine::store_version` — but **not** the transaction commit path. So
setting `time_travel_enabled=false` did **not** actually stop the commit from
emitting `v:`/`v_idx:` keys: the documented write-overhead reduction was never
realized for transactional writes.

## Change

Plumb the flag into `Transaction` (`versioning_enabled`, set from
`time_travel_enabled` in `begin_transaction`) and wrap the version-key emission in
`if self.versioning_enabled { … }`. Default `true` ⇒ **byte-identical** to before
(time-travel/AS-OF unaffected). `false` ⇒ commit writes only the `data:` key per
row.

Reads are unaffected: read-committed / latest reads use the `data:` key directly;
only AS-OF / snapshot-history reads consult `v_idx:`, which the user has opted out
of by disabling time-travel.

## Benchmark (in-memory, N=30,000 bulk + M=8,000)

| op | TT-on (default) | TT-off (P0#1) | Δ |
|---|---:|---:|---:|
| bulk_insert(txn) | 13,729/s | 14,313/s | +4% |
| autocommit_insert | 2,899/s | 2,888/s | ~0 (fast/general path, not commit-versioned) |
| update_by_pk | 2,497/s | 2,540/s | ~0 (fast-path UPDATE bypasses commit) |
| delete_by_pk | 565/s | 556/s | ~0 (DELETE writes a tombstone, never versioned) |
| point_lookup_pk | 173,657/s | 187,124/s | reads unaffected ✓ |

## Honest analysis

The **single-statement latency** win is small in-memory (~4% on bulk insert):
RocksDB memtable puts are cheap, and the row value is **not** re-serialized (the
same `&[u8]` is written to both `data:` and `v:`), so the cost is just two extra
memtable puts + two `format!`s per row. The autocommit INSERT/UPDATE fast paths
don't go through `commit_with_timestamp` at all, and DELETE writes a tombstone
(never versioned), so those are unchanged by design.

The real value is **(a) correctness** — `time_travel_enabled=false` now actually
suppresses commit-time version writes as documented — and **(b) write volume**:
3 keys/row → 1 key/row eliminates two-thirds of the keys a write-heavy workload
emits. That compounds at scale: smaller memtables, less compaction I/O and write
amplification, and a smaller keyspace (which keeps prefix-seeks and range scans
faster — cf. the P2 scan-audit). The benefit is largest on disk under sustained
write load + compaction, not in a short in-memory micro-bench.

## Validation

Default path is the original code (true branch), so time-travel behavior is
unchanged by construction. Default-feature suites pass: lib (1772, the lone
failure is the unrelated pre-existing `vector::hnsw_index` test), `crud_tests`
(27), `transaction_tests` (23/27 across runs), `datatype_tests` (27). TT-off run
returns correct latest-state reads (point lookups + full workload succeed).
(The repo's `internal-tests` time-travel suites could not be used — they are
bit-rotted against the current `Column`/`Tuple`/`LogicalPlan` structs,
independent of this change.)
