# P4.1e — Archive recovery applies runs too

Plan: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D6 "from WAL"; follows
`docs/plans/specs/P4_1d_batch_apply_per_poll.md`. Status: spec. Implementation agents never run
cargo or git.

## 1. What was measured (gate e2 on e22f3920, 2026-10-23)

`tests/physical_large_write_group.rs` brings standby B to the primary's position with
`heliosdb-nano recover --data-dir B --from-archive COPIES --through 544289`. The command ran
for 27 minutes against the test's 240 s budget and was still at the small groups before the
large one: B's RocksDB WAL grew by 9.5 MB every three minutes (~50 KB/s). The chain held 50
segments, 231 MB, ~544 000 sequences; the primary commits about seven write groups per
inserted row (Sprinter `925803b35113`), so the chain is hundreds of thousands of tiny groups.

Two causes, both in `src/replication/archive_recovery.rs`:

| path | today | cost |
|---|---|---|
| offline `recover --from-archive` → `apply_chain` (~238-250) | one `applier.apply_with(&batch, Durability::Synced)` per frame | one fsync (72 ms median on this host) and one full decode/pre-read/publish pass **per write group** |
| archive-only standby → `apply_segment` (~479-505) | one `apply_with(.., Durability::Deferred)` per frame, one `sync()` per segment | one publish pass per group (no fsync per group) |

The live receiver got run-based apply in P4.1d (`PhysicalApplier::apply_run`, one pre-read
and one publication per run, one RocksDB write per group). The archive paths did not.

## 2. The change

- Both paths collect a segment's frames into runs of at most `APPLY_CHUNK_BYTES` (32 MiB, the
  P4.1d constant, re-exported or moved where both modules can see it) — a single larger group
  is a run of its own — and call `applier.apply_run(&batches, Durability::Deferred)` per run.
  Frame verification (digest, contiguity with the cursor, `--through`) stays per frame and
  before the run is handed over; a frame past `--through` ends the chain as today.
- Durability: `writer().sync()` once per **segment** on both paths (the offline path moves
  from one fsync per group to one per segment; a crash mid-segment leaves a consistent store
  whose cursor names what was applied, and the re-run is idempotent as documented — say so
  in the module doc where "every segment verified deep before the first apply" is explained).
- The progress line of `recover --from-archive` ("applied N groups from M segments") keeps
  its meaning; add the elapsed seconds and MB/s of archive bytes to the final line.
- `run_archive_standby` (`apply_segment`): same run collection; its per-segment `sync()`
  stays where it is.

## 3. Tests

- `tests/physical_archive_recovery.rs` (or the archive_recovery unit tests): a chain whose
  segment holds 3 000 small groups is applied with exactly one fsync per segment — assert
  through `FlushMemo`-style counting on the writer if available, else through the applier's
  `Applied` statistics (runs < groups); rows equal and `range_digest` equal to a standby that
  followed live.
- `tests/physical_large_write_group.rs`: unchanged assertions; it must finish well inside its
  240 s budget in the default tier on this host (debug build). If the 50 000-row statement
  still makes the recovery of ~500 000 small groups too slow in debug, reduce the number of
  small groups the test writes before the big one (the precondition only needs the big group
  to exceed `STANDBY_POLL_BYTES`), not the big group.

## 4. Gate

Pre-commit gate (`p41gate.sh`), lint, phase gate; the P3.2 live probe
`jlive/recovery_probe.sh` (recover applied N groups from 50 segments) is re-run with the
release binary to record the new rate.
