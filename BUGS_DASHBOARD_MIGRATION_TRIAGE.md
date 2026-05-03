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

## Resolved questions (user-confirmed)

The four open questions in the original draft of this document were resolved on **2026-05-03**:

1. **Bug 1 scope** — `CREATE DATABASE` maps to the **existing tenant-management infrastructure** (`src/tenant/`, `\tenant create … db` REPL, `IsolationMode::DatabasePerTenant`). The fix is a thin SQL-DDL → tenant-API wrapper, not a new storage feature. Documented at `.claude/skills/heliosdb-nano-tenant/SKILL.md`. **No major bump needed.** A future minor release may add `CREATE DATABASE … WITH PLAN 'enterprise'` syntax to surface plan selection through SQL.

2. **Auth defaults** — `--auth trust` is acceptable **only when the listener is loopback (`127.0.0.1` / `::1`) or a Unix domain socket**. Any non-loopback `--listen` MUST require a non-trust auth method. SCRAM-SHA-256 must work as a first-class option (Bug 2 fix is mandatory; cleartext Bug 3 stays as a fallback). Implementation: at startup, if `--listen` resolves outside loopback AND `--auth trust`, refuse to start with a clear error. **Promotes Bug 2 from "high" to "release-blocker for any non-loopback deployment".**

3. **`pg_dump` round-trip** — already on the **documented upgrade path**: <https://www.heliosdb.com/docs/nano/guides/migration_guide_v220/#option-2-exportimport>. The Nano-to-Nano version-migration story relies on `pg_dump | psql` over the PG wire. **Promotes Bug 6 from "medium" to "high"** — a stalling restore breaks the documented upgrade for users coming from any earlier Nano version.

4. **Dashboard re-verification** — **batch all fixes**, then ask the dashboard team for a single re-test. No per-release back-and-forth. The triage's "Recommended next action" no longer ships Bug 8 in isolation; instead all blockers ship together as a coordinated milestone.

## Release sequencing (revised after question resolution)

| Release | Bugs | Bump | Rationale |
|---------|------|------|-----------|
| **v3.23.1** | 10, 11 (verify-and-document) + add `heliosdb-nano-tenant` skill (already merged at `41ed619` parent — landing here) | patch | CHANGELOG-only confirmation that 10/11 are closed; ships the missing tenancy skill. |
| **v3.24.0** | 8 (+ 9 auto-close) + 6 (extended-query-family completion) | minor | Bug 8 + 6 likely share the extended-query schema-synthesis root cause. Fixing them in one cycle means a single OLTP regression suite covers both. **Unblocks every prepared-statement client AND restores `pg_dump` upgrade path.** |
| **v3.25.0** | 4 (information_schema: add `tables`, `routines`, `referential_constraints`; catch-all → error-loudly) | minor | ORM-bootstrap completion. Behaviour change: clients that relied on silent-empty results now see a real error. None known. |
| **v3.26.0** | 1 (`CREATE DATABASE` → tenant-API wrapper) + 5 (StartupMessage validates DB name against tenants table) | minor | Cohesive surface — both touch the database-name plumbing. No storage layout change because tenants already have isolated namespaces. |
| **v3.27.0** | 2 (SCRAM GS2 header) + 3 (cleartext password) + same-host-only `trust` enforcement | minor | Auth correctness + safer defaults. Refusing non-loopback `--listen` with `--auth trust` is a behaviour change that warrants the minor bump. |
| **v3.27.1** | 7 (multi-statement at parser; extended + embedded paths) | patch | Low-risk parser fix. |

After v3.27.1: notify the dashboard team for re-verification on a single release that bundles **all of 1–9 closed**. If their TypeORM bootstrap runs end-to-end, the migration is unblocked.

## Recommended next action

Pick **Bug 8** as the first fix in the v3.24.0 cycle:
1. Branch `fix/extended-query-rowdescription`.
2. Phase 2 unit tests reproducing Bug 8 + Bug 6 + Bug 9 at:
   - `tests/extended_query_param_select.rs`
   - `tests/extended_query_count_distinct_param.rs` (Bug 9)
   - `tests/pg_dump_restore_smoke.rs` (Bug 6 — needs real `pg_dump` output as fixture; gate on `cfg(unix)` since `pg_dump` is a Unix tool)
3. Phase 4 bench at `benches/extended_query_bench.rs` — parameterised SELECT with varying param counts and result widths.
4. Fix in `src/protocol/postgres/handler_extended.rs` (`synthesise_schema_from_ast` fallback at line 534–550); follow-up to the SET-via-extended-query path for Bug 6.
5. Phases 3 (regression), 5 (cross-feature), 6 (OLTP head-to-head vs main).
6. Validation report `EXTENDED_QUERY_REPORT.md`.
7. Release v3.24.0.

Effort estimate: **5–8 days of engineering** including the Bug 6 follow-up and thorough cross-client testing (psql, node-postgres, psycopg, JDBC, asyncpg).
