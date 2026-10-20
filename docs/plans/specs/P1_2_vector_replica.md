# Spec P1.2 — vector indexes on a physical standby (memory-only maintenance)

Parent: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3.4 (Vidx). Sprinter `db11a1ef3b76`. After P1.1.

Value: HNSW indexes are in-memory structures. Persistent HNSW (`__vidx:{id}:*` keys, feature
`vector-persist`) is a full in-RAM copy of replicated keys whose mutators also WRITE those keys;
standard HNSW is rebuilt from rows or loaded from `vecsnap:` + `hnsw_snapshots/` files. On a
standby the keys arrive through the applier, so the in-RAM copy must follow them without writing.

## 1. Policy
`VectorIndexManager::set_persistent_policy(PersistentPolicy::ReplicaMemoryOnly)` (new enum
`{ Durable, ReplicaMemoryOnly }`, default `Durable`): in `ReplicaMemoryOnly` every mutator that
would write `__vidx:` (`on_row_insert/update/delete`, `drop_index`, `create_*`) updates memory
only and never calls the gate (which would refuse and count). `open_physical_standby` sets it.

## 2. `PersistentVectorIndex::apply_record(suffix: &[u8], value: Option<&[u8]>)`
Mirror `open`'s per-key loading (vector/persistent.rs ~711-735 and the `IndexState` fields
~116-130): `meta`, `pq`, `tomb`, `vec:{id}`, `code:{id}`, `lvl:{id}`, `adj:{id}:{level}`,
`map:`, `rmap:` — each put updates the corresponding in-RAM structure; a delete removes it.
Unknown suffix → return an error naming it (the applier logs and schedules a rebuild of that
index: `reopen_persistent_index` from the now-identical keys is always a valid fallback).
`PhysicalApplier`'s `VectorHook` (P1.1) routes `Vidx` puts/deletes here, and `Data` row changes
to `on_row_*` under the policy. Index not yet registered (its `meta:index:` arrives later or in
the same batch after the first `__vidx:` records) → buffer records per index id until
`MetaIndex` registers it, then replay the buffer; cap the buffer (e.g. 64 MiB) → rebuild instead.

## 3. Standard HNSW on a standby
Hydration ignores `hnsw_snapshots/` files and `vecsnap:` (rebuild from rows, as
`rebuild_all_indexes_for_physical_snapshot` does); `Data` row changes maintain it in memory via
`on_row_*`. `persist_index_snapshots` is already gated (P0.2).

## 4. Tests — `tests/physical_standby_vector.rs` (feature `ha-tier1`; persistent cases under
`vector-persist`)
Primary creates `hnsw` and (feature) `persistent` indexes, inserts/updates/deletes vectors, drops
an index, creates another on a populated table; after each batch the standby's kNN (top-k ids and
distances) equals the primary's; `write_gate().refusals() == 0` throughout; a crafted `__vidx:`
record with an unknown suffix triggers a rebuild, not a panic; the buffered-before-registration
case (index meta in a later batch).

## Constraints
No cargo/git; read before edit; `#[cfg(feature = "vector-persist")]` where the persistent
index lives; keep `Durable` behaviour byte-for-byte unchanged on primaries (the existing
`vector_index` tests must not change). Report files, the suffix→structure table, deviations, commands.
