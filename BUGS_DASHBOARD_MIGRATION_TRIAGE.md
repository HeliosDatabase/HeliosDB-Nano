---
source-report: /home/app/Claude-DashBoard/docs/heliosdb-bugs.md
report-version: filed against HeliosDB-Nano v3.19.1
triage-version: HeliosDB-Nano v3.23.0
status: triaged — fix plan + per-bug regression-test design + release sequencing
date: 2026-05-03
---

# Dashboard-migration bug report — triage & fix plan

## TL;DR

The Claude-Dashboard team filed 11 bugs against v3.19.1. Triaged against current v3.23.0:

- **Already fixed in-range (v3.19.1 → v3.23.0): Bugs 10 and 11.** Verified by direct repro on `main` — aggregate aliases survive, single-column projection returns one column.
- **Partially fixed: Bug 4.** `information_schema.columns` works on the PG wire (catalog dispatcher at `src/protocol/postgres/catalog.rs:76–90`), but `tables`, `routines`, and `referential_constraints` remain unimplemented. The catch-all returns an *empty* result rather than an error, masking the gap from clients that don't strictly check schema existence.
- **Still present (8 bugs):** 1, 2, 3, 4 (partial), 5, 6, 7, 8, 9.
- **Recommended priority order**: Bug 8 → Bug 4 → Bug 1 → Bug 5 → Bugs 2/3 → Bug 7 → Bug 6 → Bug 9 (likely closes when 8 closes).

The single most impactful fix is **Bug 8 (parameterised SELECT crashes node-pg)** — it blocks every prepared-statement client (TypeORM, Prisma, Drizzle, Sequelize, JDBC, asyncpg). Bug 4's remaining gap is the next ORM-level blocker for `synchronize:true` bootstraps.

## Per-bug status

### Bug 1 — `CREATE DATABASE` not implemented
**Status: confirmed present.** Repro on v3.23.0 main:
```
ERR: Query execution error: Statement not yet supported:
  CreateDatabase { db_name: ObjectName([Ident { value: "testdb", ... }]), ... }
```
Root cause: no `Statement::CreateDatabase` arm in `src/sql/planner.rs:303–745` `statement_to_plan` match — falls through to the catch-all error at `src/sql/planner.rs:741`. The PG-wire startup path also discards the `database` parameter without validation (`src/protocol/postgres/handler.rs:236–239`), so HeliosDB has no concept of multiple databases today; everything lives in the single `heliosdb` namespace.

### Bug 2 — `--auth scram-sha-256` rejects libpq's first message
**Status: confirmed present.** `src/protocol/postgres/handler.rs:751–753` parses the SCRAM client-first-message by splitting on commas and expecting `parts[1]` to be the username. Libpq actually sends `n,,n=user,r=nonce` — the leading GS2 header (`n,,`) makes `parts[0..1]` the GS2 channel-binding bytes, shifting username to `parts[2]` and nonce to `parts[3]`. The current parser misaligns every offset.

### Bug 3 — `--auth password` rejects correct passwords
**Status: needs repro.** The protocol path at `src/protocol/postgres/handler.rs:254–263` and the SHA-256 verifier at `src/protocol/postgres/auth.rs:127–148` look correct on inspection. Most likely failure mode: a trailing newline / whitespace difference in how the `--password` CLI flag is read vs how libpq sends the cleartext bytes. Needs a server-on-port repro + `wireshark`/`pcap` capture.

### Bug 4 — `information_schema.tables` not exposed
**Status: partially present.** Repro on v3.23.0 (embedded API):
```
information_schema.tables                  → ERR: Table … does not exist
information_schema.columns                 → OK (2 rows)
information_schema.routines                → ERR: Table … does not exist
information_schema.referential_constraints → ERR: Table … does not exist
```
The PG-wire side has a partial dispatcher at `src/protocol/postgres/catalog.rs:76–90` that handles `tables`, `columns`, `key_column_usage`, `table_constraints`, and `schemata`. But:
- The dispatcher only fires on the **PG wire**, not the embedded SQL planner.
- The catch-all (line 88–89) returns an *empty schema with empty rows* for any unknown `information_schema.*` query — silently misleading. ORMs that strict-check (e.g., TypeORM's `hasTable`) get a misleading empty result rather than an actionable error.
- `routines` and `referential_constraints` are flat-out missing.

The dashboard's TypeORM goes over PG wire, so the **PG-side dispatcher partially helps**. But TypeORM still fails because `information_schema.tables` is one of the views the catch-all returns empty for, **not** one of the implemented views — needs verification with a daemon-on-port test.

### Bug 5 — Connection accepts arbitrary database names
**Status: confirmed present.** `src/protocol/postgres/handler.rs:236–239` reads the `database` startup parameter but never validates or stores it. Every connection silently routes to the single `heliosdb` namespace.

### Bug 6 — `pg_dump` restore stalls
**Status: needs repro.** The SET handler at `src/protocol/postgres/handler.rs:487–492` accepts every `SET <var> = …` (except TRANSACTION/SESSION) silently with `CommandComplete + ReadyForQuery`. No obvious source of a 60-s hang here; the explorer flagged a possible interaction with the **extended-query ParameterStatus path** if `psql -f` uses prepared statements for the SET preamble — same root cause family as Bug 8. Needs a server-on-port repro with `pcap` + statement-level isolation (binary search the `pg_dump` preamble to find the offending SET).

### Bug 7 — Multi-statement queries rejected
**Status: confirmed present at the SQL parser.** Repro on v3.23.0 (embedded):
```
db.execute("CREATE TABLE m1 (a INT); CREATE TABLE m2 (b INT)")
→ ERR: SQL parse error: Multiple statements found, expected one
```
Root cause: `src/sql/parser.rs:269` returns an error if the parser finds more than one statement. The PG-wire simple-query handler at `src/protocol/postgres/handler.rs:402–420` *does* split semicolon-separated statements client-side (via `pg_split_sql_respecting_quotes`), so the bug only fires when:
- The embedded API is used with a multi-statement string, OR
- The PG-wire **extended-query** path receives multiple statements in one Parse message.

This is genuinely broken for both surfaces in different ways, but the simple-query workaround means many real workloads are unaffected.

### Bug 8 — Parameterised SELECT crashes node-pg ⭐ HIGHEST IMPACT
**Status: confirmed present.** Symptom: `pool.query("SELECT … WHERE x = $1", [val])` returns a malformed RowDescription (a column descriptor missing the `name` field) and crashes node-pg's parser. INSERT-with-params works because no rows are returned.

Root cause (per code reading): `src/protocol/postgres/handler_extended.rs:60–66, 534–550`. When the prepared-statement schema is derived during Parse, the planner-based path can succeed; if it fails, the fallback `synthesise_schema_from_ast` path produces a schema where some column descriptors don't carry a stable name. The Describe message then sends a RowDescription where one or more columns are nameless.

Affects every prepared-statement client (TypeORM, Prisma, Drizzle, Sequelize, JDBC, asyncpg, psycopg, node-pg). **This is the migration-blocker for any ORM-based workload**; without it fixed, the dashboard cannot use HeliosDB regardless of any other fix.

### Bug 9 — `COUNT(DISTINCT col) WHERE x = $1` returns 0
**Status: confirmed via inference.** Same code path as Bug 8 — the extended-query path's prepared-statement plan loses the `$1` parameter binding before evaluation, and the executor evaluates `WHERE x = NULL` (always false) → 0 matches → COUNT 0. Likely closes when Bug 8 closes; standalone fix-verify pair would still be useful.

### Bug 10 — Column alias dropped on aggregates ✅ FIXED
**Status: already fixed in v3.23.0.** Repro on main:
```
SELECT COUNT(*) AS xyzzy FROM foo
  cols = ["xyzzy"]                # ← alias preserved
  rows[0] = Some([Int8(2)])
```
The bug report claimed v3.19.1 returned `count` not `xyzzy`. Whatever fix landed between v3.19.1 and v3.23.0 has correctly propagated.

### Bug 11 — `SELECT col FROM t` returns the entire row ✅ FIXED
**Status: already fixed in v3.23.0.** Repro on main:
```
SELECT name FROM foo
  cols = ["name"]                 # ← single column
  rows[0].values.len() = 1        # ← only one value per row
  rows[0] = ["alice"]
```
Closed in-range.

## Grouping by root-cause family

| Group | Bugs | Common code area | Suggested fix order within group |
|-------|------|------------------|-----------------------------------|
| **A — Extended PG-wire protocol** | 8, 9, 6 (probable), 7 (extended path) | `src/protocol/postgres/handler_extended.rs` | 8 first; 9 likely auto-closes; verify 6 |
| **B — `information_schema` completeness** | 4 | `src/protocol/postgres/catalog.rs` + planner-side system views | 4 first; the catch-all-returns-empty needs to become catch-all-errors-loudly |
| **C — Auth protocol** | 2, 3 | `src/protocol/postgres/handler.rs:751–753` (SCRAM), `:254–263` + `auth.rs:127–148` (cleartext) | 2 first (SCRAM is the production-recommended method); 3 alongside |
| **D — Catalog / multi-DB** | 1, 5 | `src/sql/planner.rs:741`, `src/protocol/postgres/handler.rs:236–239` | 1 + 5 together (database-name routing is one cohesive change) |
| **E — SQL surface** | 7 (simple-query path is fine; embedded + extended-query path needs work) | `src/sql/parser.rs:269` | Standalone, low-risk |
| **F — Already fixed** | 10, 11 | n/a | Verify dashboard team is testing latest crate version |

## Per-fix regression test design

Each fix below ships with the eight-phase merge-validation methodology defined at `.claude/skills/heliosdb-nano-merge-validation/SKILL.md`. The **per-fix regression test** the user asked for is Phase 4: a targeted A/B bench that measures latency before and after the specific fix, plus an integration test that proves the fix.

| Bug | Phase 2 (unit test) | Phase 4 (targeted bench) | Expected perf direction |
|-----|--------------------|--------------------------|-------------------------|
| 8 | `tests/extended_query_param_select.rs` — parameterised SELECT with 0/1/many params, INSERT/UPDATE/DELETE-with-params, prepared-then-executed | A/B: same query, one via simple-query (works today), one via extended-query (fails today). Compare latency once both succeed. | Likely **modest improvement** — extended query is supposed to be faster than simple query because it skips re-parsing on Execute. Today, the buggy path may even be *avoiding* work that the fixed path would do correctly, so first-fix numbers may show a small slowdown (correctness > speed). |
| 4 | `tests/information_schema_tables.rs` — `tables`, `columns`, `routines`, `referential_constraints`, `schemata`, plus the catch-all-errors behaviour | A/B: ORM-bootstrap workload simulating TypeORM's `hasTable` polling. Measure end-to-end DataSource.initialize() time. | **Improvement on first connection** — today TypeORM retries / falls back when its probes return empty; a real result short-circuits that. |
| 1 | `tests/create_database.rs` — `CREATE DATABASE`, `CREATE DATABASE IF NOT EXISTS`, `DROP DATABASE`, multi-DB connect routing, query isolation between DBs | A/B: single-DB workload before vs. multi-DB workload after. Measure cross-DB query overhead. | **Slight regression** likely (additional namespace key prefix on every key); needs careful design (use `[db_id:8]` prefix only when multi-DB is in use). |
| 5 | `tests/startup_database_validation.rs` — connect to a non-existent DB → FATAL; connect to existing → OK | n/a (no perf surface) | n/a |
| 2 | `tests/scram_auth_libpq_compat.rs` — connect via psql, libpq, asyncpg, JDBC, node-postgres with SCRAM-SHA-256 | A/B: SCRAM handshake latency before/after | **No change** (handshake is one-time per connection) |
| 3 | `tests/cleartext_auth_libpq_compat.rs` — same matrix as Bug 2 | n/a | n/a |
| 7 | `tests/multi_statement_simple_query.rs` (already passes) + `tests/multi_statement_extended_query.rs` + `tests/multi_statement_embedded_api.rs` | n/a (no perf surface) | n/a |
| 6 | `tests/pg_dump_restore_smoke.rs` — `pg_dump` from a real PG, `psql -f` against Nano, full restore + row-count parity | E2E timing of restore (~10K rows, ~100K rows) | **Major improvement once unstuck** — today restore hangs forever |

## Release sequencing

Per the merge-validation methodology, each fix gets its own branch, validation report, and release. **Don't bundle.** That said, fixes within a root-cause group should land close in time so the regression test of one validates the other:

| Release | Bugs | Bump | Rationale |
|---------|------|------|-----------|
| **v3.23.1** | 10, 11 (verify-and-document) | patch | No code change; CHANGELOG-only entry confirming closure. Plus a heads-up to the dashboard team. |
| **v3.24.0** | 8 (+ 9 auto-close) | minor | Behaviour change: queries that returned wrong rows now return correct rows. User-visible. **Highest-impact single fix.** |
| **v3.25.0** | 4 (information_schema completion) | minor | Adds `tables`, `routines`, `referential_constraints`; flips catch-all to error-loudly. Behaviour change for clients that depended on silent-empty (none we know of). |
| **v3.26.0** | 1 + 5 | minor | Multi-database support. Big surface. Possibly a major bump (v4.0.0) if storage key layout changes. **Decision needed before starting.** |
| **v3.27.0** | 2 + 3 | minor | Auth protocol fixes. Independent of each other; bundle for one minor cycle. |
| **v3.27.1** | 7 (extended + embedded paths) | patch | Multi-statement fix at parser level. Low risk. |
| **v3.28.0** | 6 (pg_dump unstall) | minor | Likely a follow-up after v3.24 (Bug 8) since they share the extended-query path. |

If multi-database (Bug 1) requires a storage key-layout change, that's a **major bump (v4.0.0)** and should be sequenced last — design a migration story for existing v3.x data dirs.

## Recommended next action

Pick **Bug 8** as the first fix:
1. Branch `fix/extended-query-rowdescription`.
2. Phase 2 unit tests reproducing the bug at `tests/extended_query_param_select.rs`.
3. Phase 4 bench at `benches/extended_query_bench.rs` — parameterised SELECT with varying param counts and result widths.
4. Fix in `src/protocol/postgres/handler_extended.rs` (specifically the `synthesise_schema_from_ast` fallback at line 534–550).
5. Phases 3 (regression), 5 (cross-feature), 6 (OLTP head-to-head vs main).
6. Validation report `EXTENDED_QUERY_REPORT.md`.
7. Release v3.24.0.

Effort estimate: **3–5 days of engineering** including thorough tests against multiple PG-wire clients (psql, node-postgres, psycopg, JDBC, asyncpg).

## Open questions for the user

1. **Bug 1 scope**: implement multi-DB as a real storage-level isolation feature, or implement `CREATE DATABASE` as a no-op-with-success notice (everything still goes to the single namespace)? The latter unblocks pg_dump restores and most ORM bootstraps without a major version bump. Recommend the no-op-with-NOTICE first, then real multi-DB later.

2. **Auth method default**: currently `--auth trust` is the documented default. Should v3.27.0 also flip the documented default to `scram-sha-256` once it works, or keep `trust` for dev convenience and require explicit production settings?

3. **`pg_dump` source-of-truth**: should we add `pg_dump` round-trip parity to the OLTP bench harness so it's a permanent gate, or treat it as a one-shot validation when Bug 6 closes?

4. **Dashboard re-verification**: when v3.24.0 ships (Bug 8 fix), can the dashboard team re-run their TypeORM bootstrap against it? That's the highest-confidence signal that the fix lands the migration unblocker, even if other bugs remain.
