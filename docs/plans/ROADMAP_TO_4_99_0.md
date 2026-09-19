# HeliosDB-Nano — Roadmap to v4.99.0

**Status:** living document. Created 2026-09-19. **Current release:** v4.40.0 (`a4e082b`,
crates.io verified live, all 7 release jobs green). **Scope:** every item open on the sprinter
board and every open GitHub issue, sequenced into eight releases. **v4.99.0 ships when this
roadmap is empty** — no item deferred without an explicit decision recorded in this file.

**Relationship to `ROADMAP_V5.md`:** that document was written against v4.7.0 on 2026-07-27 and
last revised 2026-07-28, thirty-three releases ago. Its milestone table stops at v4.9. Its intent
— "v5.0 ships when this roadmap is empty" — is the same intent as this one, and v4.99.0 is the
version that discharges it. Treat `ROADMAP_V5.md` as historical from this commit forward; the
items in it that are still open are carried into the sections below, and the ones that shipped
are marked there already.

**Totals:** 62 tracked items (49 open, 9 triaged, 2 in progress, 2 filed while writing this),
282 effort points, 13 open GitHub issues of which 4 close with no code. Sixty-one of the 62 items
are placed in a release below; the sixty-second is GH#31, which is verified non-reproducing and
closes without one (Section 3).

## How this document was built

The sequencing below is not a sort of the backlog by priority score. It is ordered by **what one
fix makes cheap for the next**, which is what the four releases shipped on 2026-09-19 — v4.37.0
through v4.40.0, 48 items — actually demonstrated: v4.40.0's process-global sweep settled ten
questions at once and left the two remaining instances cheap, where four separate point-fixes
would have left the primitive unbuilt.

Four of the thirteen open GitHub issues were filed against 3.58.1 or 4.31.1 and had never been
re-checked. Before sequencing anything, each was **run against the v4.40.0 release binary** over
the PostgreSQL wire (psycopg 3.2.13, scratch instance, port 55432, data dir under the session
scratchpad). Section 1 records what was observed, not what the issues claim. This follows the
standing rule that a static already-fixed verdict was wrong for GH#24 and GH#27 and was nearly
wrong again for GH#31 here: **every item below closes on observed evidence or it does not close.**

Sprinter item ids are given as the 12-hex id the board uses. Effort, urgency and impact are the
board's scores; `Eff` columns below are effort.

## Section 1 — Verification pass against v4.40.0

Observed 2026-09-19 on `a4e082b`, release binary, PG wire.

| Probe | Observed on v4.40.0 | Verdict |
| --- | --- | --- |
| **GH#41** same-txn UPDATE | In-txn `SELECT` returns both inserted rows; `UPDATE … WHERE id=1` and `DELETE … WHERE id=2` each match **0 rows**, no error, post-`COMMIT` state unchanged. Updating a *pre-existing* row in the same transaction works (rowcount 1). Autocommit control works. | **Worse than filed** |
| **GH#31** VARCHAR PK unreadable | `count(*)` = 3, `SELECT *` returns all 3, `WHERE id='b2'` returns its row. All three sources of truth agree. | Not reproducible |
| **GH#42** restore reports 0/0 | Dump → restore of a 2-table, 5-row store reports `Tables: 2, Rows: 5, Constraints: 2`; reopen confirms 3 + 2 rows present. | Not reproducible |
| **GH#32** `DELETE … IN (SELECT)` | `IN subquery evaluation requires executor context. Use executor for subquery evaluation.` | Reproduces |
| **GH#39** TEXT DEFAULT now | Stored value is `'2026-09-19T14:44:08.666414734+00:00'` — literal single quotes in the bytes, plus RFC-3339 `T` at nanosecond precision where PostgreSQL emits a space and microseconds. | Reproduces ×2 |
| **GH#40** LISTEN / NOTIFY | `0A000`, message `LISTEN "chan1" is not supported by HeliosDB Nano`. The reported `XX000` and the parser-AST leak are both already fixed. | 2 of 3 fixed |
| **GH#34** expression index | `XX000` — `Column name expected in CREATE INDEX`. | Reproduces |
| **GH#33** `::vector` bound param | `XX000` — now names a workaround (`use ::vector(N) explicitly`), but the bare cast still fails on a parameter. | Reproduces |
| `781f55ba534d` NATURAL / USING | `SELECT * FROM j1 JOIN j2 USING (id)` returns 4 columns `id, a, id, b`; `NATURAL JOIN` identical. PostgreSQL returns 3. | Reproduces |
| `1703dba8e82d` TEMPORARY table | `CREATE TEMPORARY TABLE` in one session, then `SELECT count(*)` from a **second** session resolves the table and answers 0. PostgreSQL raises `42P01`. | Reproduces |
| `7127e5e46b8c` generate_series | `42883` — `Unknown scalar function: generate_series`. Correct sqlstate, function absent. | Reproduces |

### 1.1 Cross-cutting finding, not yet filed as an item

Two of the three probes that raise an error still return `XX000`. The repo already documents why
that is dangerous — `src/protocol/postgres/handler.rs` notes that poolers and HA proxies read
`XX000` as a server fault and may drop the backend — and v4.39.0 fixed exactly this for quota
refusals (`53400` / MySQL `1226`), v4.40.0 for `LISTEN` (`0A000`). The classifier gap is narrower
than it was but it is not closed. Worth one sweep over the remaining refusal paths in v4.43.0
rather than one classifier arm per issue.

### 1.2 Items filed while writing this document

- **`27bf8d819c52`** — GH#41, severity `critical`, with all four probes. This is now the board's
  only open critical; it replaces GH#31 in that slot.
- **`d408d4a1b65b`** — GH#39, with the observed stored value.

`fb6a0dae5d0f` (GH#31) and `7233cacf740b` (GH#40) were annotated with the probe evidence rather
than re-filed.

## Section 2 — The release sequence

Each release is a single mechanism, not a basket of unrelated items.

### v4.41.0 — Everything a session should own

**10 items · 44 pts.** Finishes the v4.40.0 theme and extends it one layer down, to what a
*transaction* owns. `SessionScopedState` (`src/session/scoped.rs`) and
`StorageEngine::instance_id()` already exist, so the two remaining process-global instances are
now cheap. The new critical belongs here because it is the same shape one layer down: the write
path reading state the read path scopes correctly.

| Item | Sev | Eff | What |
| --- | --- | --- | --- |
| `27bf8d819c52` | critical | 5 | GH#41 — UPDATE/DELETE blind to the transaction's own inserts |
| `564e9ac1d762` | high | 4 | Advisory locks not per-database — one db's Prisma migration lock blocks another |
| `32ed4b9e0002` | high | 3 | `pg_stat_activity` lists every database's backends |
| `0d6695bf8a86` | high | 5 | Session-less params from wire, REST and MCP join the global transaction |
| `1703dba8e82d` | high | 4 | `CREATE TEMPORARY TABLE` creates a permanent, globally visible table |
| `0823e2fe7603` | medium | 6 | `session_txn_count` is process-wide — one open txn disables every fast path |
| `e4bb83a1afa0` | medium | 5 | `CALL` runs its procedure body outside the enclosing transaction |
| `37a5968e7698` | medium | 4 | Savepoint stack is process-wide and survives ROLLBACK/COMMIT |
| `afed6c8e8d1d` | medium | 4 | Extended-protocol SAVEPOINT / RELEASE / ROLLBACK TO error inside a session txn |
| `f469f178aa29` | medium | 4 | Residue: MCP SSE session namespace, per-engine config statics |

**Gate note.** `27bf8d819c52` needs a both-families corpus test, not a single pin: per the
dual-dispatch hazard, census every DML arm (text `execute`, `parameterized_plan_cached`,
`query_params_with_schema`, batch, and the fast UPDATE/DELETE shortcut arms).

### v4.42.0 — Access control that enforces

**5 items · 29 pts.** The largest concentration of risk on the board, and the release most bound
by the standing rule that **when shipped behaviour and the docs disagree, build the feature**. The
MySQL listener trusts every connection while the docs promise SCRAM; `GRANT`/`REVOKE` are parsed
and discarded.

**This must follow v4.41.0, not run beside it.** Enforcement against process-global identity is
enforcement that is *wrong*, not merely absent. That is not theory: v4.39.0's tenant metering was
written, tested and deliberately withheld because charging a connection's statements to whichever
tenant another connection last selected is worse than not charging at all. It shipped unchanged in
v4.40.0 once the per-session tenant binding existed.

| Item | Sev | Eff | What |
| --- | --- | --- | --- |
| `a32c758339f5` | high | 4 | GH#38 — auth config file ignored; `--password` leaks into `/proc/*/cmdline` |
| `90c19c945b91` | high | 6 | MySQL listener authenticates with trust regardless of `--auth` |
| `a061438002d6` | high | 6 | RLS does not walk CTEs, UNIONs or table functions; scalar subqueries run RLS-blind |
| `5452ec95732d` | high | 8 | GRANT/REVOKE accepted but never enforced |
| `d12711c45b80` | medium | 5 | REST/MCP/BaaS run session-less: `current_user`, `is_superuser` wrong |

### v4.43.0 — Executor and PostgreSQL parity

**10 items · 38 pts.** Wrong answers rather than refusals. The anchor is the params family running
none of the five optimizer passes — the same divergence that hid the fast-DELETE and fast-UPDATE
defects in v4.38.0 — so this release needs a both-families corpus test, not per-item pins. Four of
these reproduce live (Section 1). Fold the `XX000` classifier sweep from §1.1 in here.

| Item | Sev | Eff | What |
| --- | --- | --- | --- |
| `1c822fec6c1b` | high | 3 | `query_params()` runs no optimizer passes while the other two families run all five |
| `6f62088f17a6` | high | 5 | GH#32 — `DELETE … WHERE col IN (SELECT …)` fails |
| `b96bc6b51ae5` | high | 5 | `SELECT <unknown col>` from an empty table succeeds with 0 rows instead of 42703 |
| `780c04186469` | medium | 5 | Branch `created_at` renders the MVCC tick as a 1970-relative timestamp |
| `9dbe35eeec48` | medium | 4 | Mojibake heuristic rewrites legitimately stored Latin-1-shaped text |
| `ecf8d5a9962d` | medium | 4 | Trigger bodies are always empty at creation time |
| `781f55ba534d` | medium | 3 | NATURAL JOIN / USING do not merge the join column |
| `d408d4a1b65b` | medium | 2 | GH#39 — TEXT `DEFAULT CURRENT_TIMESTAMP` stores surrounding quotes |
| `9e843156d337` | low | 4 | INT overflow promotes to BIGINT instead of raising 22003 |
| `deada0f71df8` | low | 3 | Extended protocol defers RETURNING-bind refusals from Parse to Execute |

### v4.44.0 — Drivers, catalog, wire

**8 items · 40 pts.** What breaks a real client before it reaches SQL: pgvector init, `psql`
introspection, driver type resolution. `671743292162` already has its tripwire test in the tree
from v4.39.0 — it flips green the moment `DataType` gains `char` and `oid`, which is the
prerequisite this release builds.

| Item | Sev | Eff | What |
| --- | --- | --- | --- |
| `0bb12717c01e` | high | 8 | GH#33 — `::vector` cast fails on a bound parameter |
| `a267c36c4fdf` | high | 8 | GH#34 — expression index rejected; blocks mem0 pgvector init |
| `7127e5e46b8c` | medium | 6 | Set-returning functions in the SELECT list (`generate_series`) |
| `80608e44150a` | medium | 5 | Dump + `information_schema` rendering gaps |
| `671743292162` | medium | 3 | vector OID: wire advertises 1000 (`_bool`), catalog says 3614 |
| `c17b9355da55` | low | 4 | Catalog follow-ups: `\dT` / `\dD` error, enum types, interceptor disagreement |
| `a18861fe6fa4` | low | 3 | `pg_tables` interceptor: alias-qualified columns and IN-subqueries answer zero rows |
| `fe0a4ca83293` | low | 3 | FTS follow-ups: `@@` is OR-only, tsvector accepts any JSON shape |

### v4.45.0 — Durability and operations

**7 items · 28 pts.** What a backup, an upgrade and a crash are worth. The dump checksum is
computed before the header is stamped and never verified on restore, which makes every other
guarantee here conditional. Sequenced after the executor work so the HA standby test measures a
settled write path.

| Item | Sev | Eff | What |
| --- | --- | --- | --- |
| `7f10caa5de3f` | high | 5 | Dump checksum computed pre-header and never verified; appending is undetected |
| `d0b1506ef709` | high | 5 | GH#37 — image uid 999 vs 3.x data uid 0; in-place upgrade fails with no diagnostic |
| `a3ffb9ef3c2c` | medium | 6 | Embedded explicit transactions vs HA standby — needs an integration proof |
| `f2746048d395` | medium | 4 | No clean-shutdown marker: recovery cannot tell a clean stop from a crash |
| `0a67a0637345` | medium | 3 | `CREATE SEQUENCE … START WITH n` silently ignored when it already exists |
| `e3bd958c2327` | low | 3 | Restore follow-ups: no lenient restore in the library API, phase-B discards warnings |
| `20454abcf9b1` | low | 2 | Controlled removal of the `--skip ha_tests::streaming_tests` workaround |

### v4.46.0 — Performance and dead weight

**10 items · 48 pts.** Deliberately late: every item above changes the hot path, and measuring
before they land buys a number that expires. Two of these are pure removal — the bloom/zone
builders and the predicate-pushdown write path both have zero callers while the docs describe them
as working. `8a0c7200f1e4` carries `ESCALATE: user` and blocks this release (Section 4).

| Item | Sev | Eff | What |
| --- | --- | --- | --- |
| `f37602371489` | high | 4 | A2 literal normalizer is a 713× pessimization on repeated identical reads |
| `175366bbb514` | high | 5 | `can_push_predicate` rejects Parameter — no wire read ever reaches FilteredScan |
| `8a0c7200f1e4` | medium | 5 | GH#21 regression review — owner verdict required |
| `059e7b06dc4a` | medium | 7 | Re-measure wire-to-wire vs PostgreSQL after W1/W2 |
| `a567fe2262a6` | medium | 6 | PQ index build time and the undocumented 960-vector training minimum |
| `bc0b278861e5` | medium | 5 | SMFI is dead code: bloom/zone builders have zero callers |
| `a5551271b3d9` | medium | 5 | `ci_perf_smoke` is layout-sensitive at ±5% on this host |
| `cb5b6415b196` | medium | 4 | BETWEEN is not pushed into the columnar scan kernel |
| `fd82221ec311` | medium | 4 | ALTER TABLE is the one pg35 category PostgreSQL wins (922µs vs 593µs) |
| `9e61e79d87ca` | medium | 3 | `PredicatePushdownManager` write path has zero callers |

### v4.47.0 — The promises

**6 items · 49 pts.** The long pole, and the only release that is a **scope decision rather than a
work plan**. Six features the documentation already describes as working. At effort 8–9 each this
is 10% of the items and 17% of the effort, but it is a different *kind* of work — new subsystems,
not repairs — and it is where a date will slip. The standing rule says build rather than retract,
so the decision to take is scope and sequencing, not whether (Section 4).

| Item | Sev | Eff | What the docs already promise |
| --- | --- | --- | --- |
| `bac656f50255` | high | 9 | Trigger bodies execute (`CREATE TRIGGER` is accepted today and does nothing) |
| `71eeb50b70cc` | medium | 9 | PL/pgSQL user-defined function bodies |
| `6bfd3203c254` | medium | 9 | Transactional DDL — DDL inside BEGIN/ROLLBACK is not rolled back |
| `ee7425a24351` | medium | 9 | True SERIALIZABLE (SSI) — the label is served as snapshot isolation |
| `7233cacf740b` | medium | 8 | LISTEN / NOTIFY / pg_notify — ada-core polls every 250 ms instead |
| `35e6cf9072c0` | medium | 5 | Reopen posture for an unenforceable UNIQUE: record and block, never silently drop |

`ecf8d5a9962d` (v4.43.0) is the parser half of `bac656f50255` and must land first: trigger bodies
are discarded at creation time, so there is nothing for an executor to run until that is fixed.

### v4.99.0 — Finalized

**5 items · 6 pts, plus hygiene.** Cheap by construction: everything left is bookkeeping, and the
release exists to make one claim — **zero open items on the board, zero unreconciled promises in
the docs**. Its gate is the claim, not the items.

| Item | Sev | Eff | What |
| --- | --- | --- | --- |
| `a6c0cee55d28` | medium | 2 | GH#29 — confirm the verified-fixed list in the release notes *(in progress, opencode)* |
| `cf485bb0538e` | low | 1 | PGConf doc corrections #8–#11 — blocked on doc-source location |
| `bbca260fe065` | low | 1 | Untracked `CLAUDE.md` + plan doc on main |
| `f68a32988102` | low | 1 | Cleanup owed: two agent tables left in `heliosdb-lite-ca` |
| `5f0c324701e6` | low | 1 | sprinter `HEAD /login` and `/healthz` return 405 |

Plus, not tracked as items: remove the `src/multi_tenant/` dead subsystem; close GH#31, #35, #36
and #42 (Section 3); reconcile every doc claim against shipped behaviour; retire
`docs/plans/ROADMAP_V5.md`.

## Section 3 — Close on GitHub now

Four of thirteen open issues need no code. Each needs a reply to its reporter before closing, and
per the rule that burned GH#24 and GH#27 the two non-reproducing ones should be re-run with **the
reporter's exact original script**, not the reduced shapes in Section 1.

| Issue | State | Evidence |
| --- | --- | --- |
| GH#35 | Fixed, not closed | Logical-WAL replay dropping tables on open. Shipped as `b32f364`, merged `976a916`. |
| GH#36 | Fixed, not closed | RenameTable in the logical log. Shipped as `8964bc7`; `WalOperation::RenameTable` is mapped in `src/replication/ha_state.rs:545`. |
| GH#31 | Verify, then close | Was the board's only `critical`. All three read paths agree on v4.40.0. |
| GH#42 | Verify, then close | Restore reports correct table and row counts and the data is present on reopen. |

Closing these takes the externally visible count from 13 to 9.

Remaining open issues map to releases as: #38 → v4.42.0; #32, #39 → v4.43.0; #33, #34 → v4.44.0;
#37 → v4.45.0; #40 → v4.47.0; #41 → v4.41.0; #29 → v4.99.0.

## Section 4 — Blocked on the maintainer

### 4.1 v4.47.0 scope — blocks the meaning of v4.99.0

Triggers, PL/pgSQL, transactional DDL, SSI, LISTEN/NOTIFY. Each is a subsystem, not a fix. Build
all six and v4.47.0 is the schedule; build the two the docs are loudest about (triggers,
LISTEN/NOTIFY) and defer the rest past v4.99.0, and the board closes far sooner — but then
**v4.99.0 cannot claim the docs are reconciled**, which is the whole point of the version. The
standing rule rejects retracting the claims, so the real choice is between a later v4.99.0 and a
narrower promise set. Record the decision here when it is made.

### 4.2 GH#21 regression acceptance — blocks v4.46.0

`8a0c7200f1e4` carries `ESCALATE: user`. The candidate reports 35/35 category wins yet retains 20
pooled slowdowns against latest and 19 against original, 11 and 17 of them above 3%. It is frozen
pending an owner verdict on whether that is acceptable. It blocks nothing until v4.46.0, then
blocks it.

## Section 5 — What this sequence is betting on

Three assumptions, each falsifiable, two of which have already been wrong once.

**The estimates.** 282 effort points across 62 items. The four releases shipped on 2026-09-19 —
v4.37.0 through v4.40.0, 48 items — suggest the unit is achievable in batches, but every one of
those was drawn from the easy-to-medium end. What remains is the tail: mean effort here is 4.6
against the 2.4 of the v4.38.0 batch. Do not read the shipped cadence as the forecast.

**The failure mode to watch.** Every defect found during those four releases that mattered was
found by running the gate, not by reading the code — the tenant metering that silently disabled a
working charge site, the sequence re-key that updated an identity's consumers and missed its
producer, two agents building different primitives for one job with an ABA hazard in one of them.
Static already-fixed verdicts were wrong for GH#24 and GH#27 and nearly wrong again for GH#31
here.

**The ordering claim.** v4.41.0 before v4.42.0 is the load-bearing decision. If it is wrong — if
access control can be scoped correctly without the session work — the two can run in parallel and
the schedule shortens by a release. The evidence it is not wrong is v4.39.0, recorded above.
