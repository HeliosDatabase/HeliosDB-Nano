# R3 — rewind an old primary by fast-forward; `SWITCHOVER TO` (Sprinter 1c8963417a35, bbba90ddfc61)

Plan: `docs/plans/STANDBY_BACKLOG_4_42.md` (on branch fix/4420-standby-backlog). Read first:
`docs/plans/PHYSICAL_STANDBY_REPLICATION.md`, `docs/plans/specs/P4_*` (recover ladder:
tail → incremental fast-forward by leaf digests → full copy), `docs/plans/specs/P5_1_epoch_promote_fence.md`
(stored epoch, fence_self/PrimaryFence, offline `promote`, `recover --from-primary` routes,
§6 rebuild of a fenced primary by FULL COPY with --replace) and
`docs/plans/specs/P5_2_pg_promote_online.md` (online promotion: Promoter, StandbyHost,
ReceiverCommand::Fence over the live session, in-process reopen as primary, session
termination via the session registry). Oracle vocabulary the maintainer thinks in: a
Data Guard SWITCHOVER converts both roles in one command; a FAILOVER plus REINSTATE
DATABASE (flashback-based) brings the old primary back as a standby.

## R3a — rewind (1c8963417a35)

Today a fenced or superseded primary becomes a standby only through `recover --from-primary
--replace` = a FULL COPY, because the fast-forward stage (`ReopenedPhysicalStage::open`)
needs a physical marker, a manifest and a `__nano_local` cursor that a primary directory does
not have. Its data is byte-identical to the new primary's up to the fork (same history), so
only what diverged needs to move.
- `recover --from-primary` on a STOPPED directory carrying `NANO-PRIMARY-HISTORY.json` of the
  SAME history as the source and a stored epoch LOWER than the source's (fenced or not; a
  directory under a pending promotion record is refused as today) converts it into a physical
  stage in place: synthetic marker + manifest + `__nano_local` with a cursor at an UNKNOWN
  position (so no tail is attempted), drops primary-only local state (logical-WAL redo keys,
  the local history file, `NANO-FENCED.json` — keep a copy of the fence record in the
  recovery record for diagnosis), then runs route 2 (incremental fast-forward: every leaf
  decided against the source's pinned snapshot; differing leaves shipped as SSTs; the
  existing crash-resumable recovery record) and leaves the directory a standby of the source.
- `--replace` keeps its meaning for the full-copy fallback only; the rewind itself is in
  place. Fall back to the full copy (as today) when the directory is damaged, the history
  differs, or the divergence exceeds `--full-copy-threshold`; say which and why.
- Tests (tests/physical_fast_forward.rs or a new file): a forced promotion
  (`promote --force` with the old primary still running and taking writes after the fork, so
  the two DIVERGE), then rewind the old primary: it starts as a standby of the new primary,
  its replicated keyspace digest equals the new primary's at the same sequence, the rows
  written on the old primary after the fork are gone, and the bytes shipped are a small
  fraction of the dataset; a kill during the leaf apply resumes; a different history is
  refused; an equal-or-higher epoch is refused.

## R3b — `SWITCHOVER TO` (bbba90ddfc61)

Today `SWITCHOVER TO '<node>'` (parser.rs ~4092) is parsed and refused. Make it a planned role
swap run on the PRIMARY, built only from the P5 pieces:
1. `SWITCHOVER TO '<target>'` (superuser; `<target>` = a registered standby's node name or its
   replication address as the primary's `/health` standby list shows it; `SWITCHOVER CHECK
   TO '<target>'` = the preflight only, returning a row per check). Preflight: the target is
   attached and `streaming` within `max_standby_lag`, supports online promotion (advertise a
   capability bit in the target's Hello if none exists), the target's replication address is
   reachable from this node (this node will follow it), and this node can be demoted (it has
   what a standby start needs: the target's address, the token, TLS settings consistent with
   the pair's — reuse P5.2's downgrade refusal rule).
2. Ask the target to promote: a new control frame from the source to that receiver over its
   live session (e.g. `PromoteRequest { epoch: E+1, requested_by, deadline }`); the target
   runs its normal online promotion (P5.2 `Promoter`), whose fence comes back through the
   same session — this primary fences itself (P5.1 `fence_self`: write gate fenced, sessions
   57P01, archiver sealed through `final`), the target catches up to `final`, converts and
   serves as the primary at epoch E+1.
3. Demote this node in-process (the mirror of P5.2's standby→primary reopen): keep sessions
   terminated, stop the replication source/archiver services, release the store, run R3a's
   rewind against the new primary (no divergence is possible after a clean fence, so the
   fast-forward should ship nothing beyond the tail), reopen the store as a physical standby
   following the new primary, posture `standby` (so `pg_is_in_recovery()` = t,
   `in_hot_standby` = on, `/ready` follows the standby rule). Its configuration on disk is
   updated so a restart starts it as that standby (role, primary address) — say exactly
   which file/keys, and keep the operator's original config file untouched (write an
   override next to the data directory if needed, and document it).
4. Answer the caller (its session ends with 57P01 like a promotion's). Failure handling: any
   failure BEFORE the target accepted → nothing changed, error 55000 naming the reason; after
   the target promoted → this node stays fenced (never two writable primaries), reports the
   command to finish it (`heliosdb-nano recover --from-primary --data-dir …` then `start`),
   and logs at ERROR. Bounded waits everywhere (reuse P5.2's bounds).
5. Docs: physical-standby guide (SWITCHOVER section; Oracle mapping table row), CHANGELOG,
   server skill, and `replication-history-guard.md`'s "Promotion is refused" text.
- Tests (tests/physical_switchover.rs, daemon pair, plain and TLS): `SWITCHOVER CHECK`
  rows; a switchover with continuous writes before it: the target is the primary at epoch
  E+1 with every acknowledged write, the old primary is its streaming standby (digests equal
  at the same sequence), sessions on both got 57P01 and redial; a second switchover back
  restores the original roles; a target that is lagging/disconnected → refused with nothing
  changed; a target killed after accepting → the old primary stays fenced and says what to
  run. Proxy contract: the old primary's posture moves fenced → standby; the new primary's
  timeline increases (the Proxy session will drill it).

## Rules

Do not run cargo, git or any build; the dispatching session runs every build and gate. No new
`unwrap`/`expect` on non-test paths. Do R3a first and report; R3b builds on it. Report: files
touched, the conversion steps and what is kept/dropped, the new frame(s) and capability bit,
the demotion steps, failure handling, tests and what each proves, and anything in this spec
that turned out wrong.
