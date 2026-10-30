# R1 — standby tooling, guards and residuals

Plan: `docs/plans/STANDBY_BACKLOG_4_42.md`. Base: the physical standby (P0-P5) as built on this
branch; read `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` and the P4/P5 specs under
`docs/plans/specs/` (P4_*, P5_1_*, P5_2_*) for the protocol and the recover machinery.

## 1. `heliosdb-nano standby verify` (plan "Verification" §3)

Prove a standby's replicated keyspace is byte-identical to its primary's at the same
sequence. Today `range_digest` (src/replication/physical.rs ~1061) exists but only tests use
it.
- Command: `heliosdb-nano standby verify --data-dir D [--against HOST:PORT]
  [--replication-token-file F] [TLS flags as recover] [--json]`, on a STOPPED standby
  directory (refuse a running one by its lock, like `recover`). `--against` defaults to the
  configured primary.
- Method: reuse the fast-forward machinery of `recover --from-primary` (P4): open the session
  (Hello, history, epoch), have the primary pin a snapshot at sequence P, bring the standby
  to exactly P by tailing (the same tail route recover uses; if the primary cannot serve the
  tail — pruned — refuse with the recover command to run), then compare the per-leaf digests
  of the replicated keyspace at P on both sides WITHOUT shipping anything. Report
  "identical at sequence P (N leaves, X bytes compared)" and exit 0, or list the differing
  leaf ranges (first/last key, both digests) and exit 1. Node-local state (`__nano_local`) is
  excluded by construction.
- It must never write the standby beyond applying the tail a normal start would apply, and
  never modify the primary.
- Tests: identical pair → exit 0; a standby whose replicated keyspace is altered directly
  (test-only writer to the default family) → exit 1 naming the leaf; a running standby →
  refused; a primary that pruned the needed tail → refused with the recover hint.

## 2. `recover --from-primary` must not adopt a stale completed copy (6146b39a5beb)

`Ladder::full_copy` → `completed_copy` (recover_primary.rs ~1349) adopts a leftover
`<D>.recover-full` by history only. Before adopting, probe the primary with Resume at the
copy's sequence: accepted → adopt; WalPruned / gap / other refusal → log "the completed copy
at … (sequence …) is older than what the primary retains; copying again", remove it, copy
fresh. Under `--full`, copy fresh unless the completed copy is still served (then say it is
being adopted). When nothing was copied in this run, the summary says "(adopted the
completed copy at …, nothing copied in this run)" instead of "0 B". Test in
tests/physical_fast_forward.rs: a stale completed copy is discarded and re-copied; a fresh
one is adopted.

## 3. Key rotation must not strand a standby (plan §D5)

A standby holds the dataset key; ciphertext is shipped verbatim. Find every key-rotation entry
point (src/crypto/key_manager.rs `rotate`, and any re-encryption path in src/storage/tde.rs
or elsewhere; check whether any CLI/SQL/HTTP surface reaches it). Make rotation fail closed
while it would strand a replica: refuse on a physical standby (its key must come from its
primary), and on a primary while any physical standby is registered/attached, with a message
naming the standbys and the procedure (rotate on a primary without standbys, then rebuild or
re-key the standbys) — unless a future capability lets standbys hold both keys. Unit/integration
test for each refusal. If rotation has no reachable surface at all, still add the guard at
the lowest common entry point and say so in the doc.

## 4. Re-triage the residual items under physical replication

- e21903322db7 (REPLICATION_4_41_1_DESIGN.md §7-8 residuals: LSN continuity, numeric-LSN ACK
  identity, `log_*` outside capture frames, autocommit DDL logged after the catalog change,
  group-commit measurement). For each point decide with evidence whether it still applies now
  that standbys are physical (the logical WAL is only the primary's own redo window; ACKs are
  on the physical sequence; the receiver refuses sequence gaps, 8d1575a8, and verifies the
  last group, NPHYS004). For any point that still affects the PRIMARY's own crash recovery
  (logical WAL redo), fix it or prove it harmless with a crash test. Write the verdicts into
  `docs/plans/STANDBY_BACKLOG_4_42.md` (an "R1 re-triage" section) with file:line evidence.
- a3ffb9ef3c2c (explicit transactions reach a standby): add a test on the physical pair
  harness (tests/support/physical_pair.rs): explicit multi-statement transactions on the
  primary over the PG wire (simple and extended protocol, with a ROLLBACK and a SAVEPOINT
  case) arrive on the standby, and `range_digest` of the replicated keyspace is equal at the
  same sequence. Note in the doc that replication is API-independent (RocksDB WAL).

## 5. Plan doc

Correct `docs/plans/PHYSICAL_STANDBY_REPLICATION.md`: the P5 row and §D5 step 2 (HNSW dump
NOT moved into RocksDB, by decision, with the reason); mark the delivered verification item
(`standby verify`); leave decision 7 (cut markers) to R2.

## Rules

Do not run cargo, git or any build; the dispatching session runs every build and gate. No new
`unwrap`/`expect` on non-test paths. Report: files touched, the verify protocol and output,
each re-triage verdict with evidence, the tests and what each proves, and anything in this
spec that turned out wrong.
