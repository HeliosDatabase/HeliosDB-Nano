# Spec P0.1 — keyspace classification, statvfs disk helper, byte-size parser

Parent: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` (phase P0). Sprinter `e707d7b4dd28`.
Branch `feat/physical-standby`. Three independent, low-risk foundations. No behaviour change
for users; the added value is the two CI tripwires that every later phase relies on.

## 1. `src/storage/keyspace.rs` (new) — one authoritative map of every RocksDB key prefix

```rust
/// Every key class the engine writes to the default column family. All of them are
/// REPLICATED verbatim to a physical standby; node-local state never uses the default CF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyClass { Data, Version, VersionIdx, Snapshot, Vmeta, MetaTable, MetaIndex, MetaSchema,
    MetaSequence, MetaSeqState, MetaMv, MetaRole, MetaAcl, MetaFunction, MetaProcedure, MetaDelta,
    MetaTdeKeyCheck, TableConstraints, Trigger, TriggerRowMut, Statistics, Compression, ViewMeta,
    SystemConfig, Counter, LegacyCounter, Dict, Columnar, ColumnarZ, ColumnarP, ColumnarPm,
    ColumnarZm, Cas, Delta, Branch, BranchData, BranchDel, BranchVersion, Git, Vidx, IdxSnap,
    IdxSnapMarker, VecSnap, VecSnapMarker, WalEntries, WalLastLsn, WalCheckpoint, WalPrunedThrough,
    ReplAppliedLsn, PhysicalCursor, VgcWatermark, VgcAnchor, ElideSentinel, ChangeLog, ChangeIndex,
    ChangeMeta, Unknown }

pub struct Classified<'a> { pub class: KeyClass, pub table: Option<&'a str> }
pub fn classify(key: &[u8]) -> Classified<'_>;
/// Every (literal prefix, class) pair, in match order. The scan test reads this.
pub const PREFIXES: &[(&[u8], KeyClass)];
```
- Build `PREFIXES` from the real constructors; read each before writing the literal:
  `data:` (engine.rs `build_data_key` ~2242), `v:`/`v_idx:`/`snapshot:`/`vmeta:` (time_travel.rs
  20-110, copy_marker.rs 8-11), `meta:*` (catalog.rs 518, 1782, 4365-4909), `table_constraints:`,
  `trigger:`, `trigger_rowmut:`, `statistics:table:`, `compression:config`, `__view_metadata__`,
  `system:config`, `counter:`/`meta:counter:` (engine.rs ~2217), `dict:` (dictionary.rs 124),
  `col:`/`colz:`/`colp:`/`colpm:`/`colzm:`/`cas:` (columnar.rs 483-500, tde.rs 109-131), `delta:`
  (mv_delta.rs 369), `branch:*`/`bdata:`/`bdel:`/`bv:`/`git:` (branch.rs 23-34, 463), `__vidx:`
  (vector/persistent.rs 300-304), `idxsnap:`/`idxsnapv:`/`vecsnap:`/`vecsnapv:` (index_snapshot.rs
  79-82), `wal:entries:`/`wal:last_lsn`/`wal:checkpoint`/`wal:pruned_through` (wal.rs 1179-1192),
  `repl:applied_lsn` (engine.rs ~11412), `__nano_physical_replica_cursor_v1` (physical.rs 13),
  `vgc:low_watermark`/`vgc:anchor:` (version_gc.rs 67-69), `w3_2_elide_used` (time_travel.rs 52),
  `change_log:`/`change_index:`/`change_meta:` (sync/change_log.rs 18-23, feature-gated: classify
  them unconditionally). Longest prefix wins (`meta:counter:` before `meta:`, `v_idx:` before `v:`).
- `table` is extracted for `Data`, `Version`, `VersionIdx`, `Vmeta`, `Counter`, `BranchData`,
  `BranchDel`, `BranchVersion` using the SAME split the engine uses (`extract_table_from_key`
  engine.rs ~2199 splits at the first `:` after the prefix; row ids are decimal; `v_idx:{t}:{id}:{rev}`).
  Do not invent a new split; call or mirror the existing helpers and say which in a doc comment.
- Unit tests: one key per class round-trips; longest-prefix cases; `Unknown` for `zzz:`.

### Tripwire test A — every key literal in `src/` is classified
`tests/keyspace_classification.rs`: walk `src/**/*.rs` (skip `#[cfg(test)]` blocks is NOT
required — scan everything), collect every string literal that looks like a key prefix:
`format!("<ident>:` … , `b"<ident>:`, `"<ident>:"` where `<ident>` is `[A-Za-z_][A-Za-z0-9_]*`
and the literal is used as a key (heuristic is fine: accept any literal matching `^[a-z_]+:` or
`^__[a-z_]+` found inside `format!(`, `b"`, or `.as_bytes()` contexts). Also list the FULL fixed
keys (`wal:last_lsn`, `vgc:low_watermark`, `w3_2_elide_used`, `__nano_physical_replica_cursor_v1`,
`meta:tde:keycheck`, `compression:config`, `__view_metadata__`, `system:config`, `branch:registry`,
`branch:pending_gc`). Assert `classify(literal) != Unknown` for each, printing the file:line of
every failure. Keep an explicit `ALLOW: &[&str]` for literals that are provably NOT keys (e.g. SQL
text, URL schemes like `http:`), each with a one-line reason; keep it short.
Red/green: run it first against the tree with an intentionally missing prefix to prove it fires.

### Tripwire test B — a primary's WAL contains no Unknown key
`tests/keyspace_wal_corpus.rs`: open a temp `EmbeddedDatabase` (persistent, WAL on,
`time_travel_enabled` default), run a corpus that touches every subsystem: CREATE TABLE (incl. a
TEMP table and a table with UNIQUE/FK), INSERT/UPDATE/DELETE, CREATE INDEX (btree + HNSW
`USING HNSW` persistent), CREATE SEQUENCE + nextval, CREATE VIEW, CREATE MATERIALIZED VIEW +
REFRESH, CREATE FUNCTION/PROCEDURE, CREATE TRIGGER, CREATE DATABASE BRANCH + USE + MERGE, COPY,
TRUNCATE, ALTER TABLE ADD COLUMN, DROP TABLE, a transaction with SAVEPOINT, time-travel `AS OF`,
then clean close (so index snapshots and counters flush). Then open the RocksDB read-only
(`rocksdb::DB::open_for_read_only`) and iterate `get_updates_since(0)`, decoding every put/delete
key with `rocksdb::WriteBatchIterator` (`WriteBatch::iterate`; note it drops range-deletes — also
iterate the full keyspace with a prefix-less iterator and classify every live key). Assert zero
`Unknown`, printing offenders. Use the existing test helpers for temp dirs/config (look at
`tests/physical_checkpoint_tests.rs` and `tests/gh_issue_35.rs` for the persistent-open pattern;
`storage.wal_enabled` must be true, which is the default for persistent stores — verify).

## 2. `src/storage/disk.rs` (new) — `free_bytes` via statvfs
```rust
/// Free bytes available to an unprivileged writer on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> io::Result<u64>   // libc::statvfs: f_bavail * f_frsize
```
- Replace the body of `StorageEngine::check_disk_space` (engine.rs 2144-2172) with a call to it,
  keeping the error text and the "skip the check on error" behaviour (log at debug once).
  Fix the stale doc comment there (it claims statvfs already).
- Tests: `free_bytes(tempdir) > 0`; non-existent path is an error; the engine check still
  refuses when the helper reports below `MIN_DISK_SPACE_BYTES` (inject via a `#[cfg(test)]`
  override or by factoring `check_disk_space_with(available: u64)`).

## 3. `config::parse_byte_size` — "16GiB" style sizes
```rust
/// "123" (bytes), "512K"/"512KiB"/"512KB", "16M"/"16MiB", "2G"/"2GiB", "1T"; case-insensitive;
/// binary units (K = 1024). Also accepts "unlimited" -> None at the call sites that allow it.
pub fn parse_byte_size(raw: &str) -> crate::Result<u64>
```
next to `parse_retention_duration_secs` (config.rs ~904), same error style ("invalid … expected …").
Tests: each unit, whitespace, overflow error, empty error. Not wired to any config field yet.

## Constraints
- Match the surrounding style (doc comments explain WHY; error messages name the setting).
- No `cargo`, no `git`: report the files you changed and the exact commands I should run.
- Nothing outside `src/storage/keyspace.rs`, `src/storage/disk.rs`, `src/storage/mod.rs`
  (re-exports), `src/storage/engine.rs` (check_disk_space only), `src/config.rs`, the two new test
  files, and `tests/` helpers if strictly needed.
- Fail closed: never make `classify` guess a table from a malformed key — return `table: None`.
