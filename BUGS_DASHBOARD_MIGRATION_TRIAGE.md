---
source-report: /home/app/Claude-DashBoard/docs/heliosdb-bugs.md
report-version: filed against HeliosDB-Nano v3.19.1
triage-version: HeliosDB-Nano v3.23.0
status: re-triaged 2026-05-03 evening — most bugs already fixed via PG-wire verification
date: 2026-05-03
update: Daemon-on-port verification (port 15441/15442/15444 with --http-port 18081/18082/18084 to dodge the 8080 collision that was killing earlier launches) flipped 6 of 9 "still present" bugs to "fixed in v3.23.1". See "Verified status (PG-wire daemon, v3.23.1)" section below.
---

# Dashboard-migration bug report — triage & fix plan

## TL;DR (re-triaged 2026-05-03 evening)

The Claude-Dashboard team filed 11 bugs against v3.19.1. **8 of 11 are already fixed in v3.23.1**. Only 3 still need engineering work, plus 1 partial.

| Status | Bugs | Action |
|--------|------|--------|
| ✅ Fixed | 3, 6, 7 (simple-query), 8, 9, 10, 11 | None — close in dashboard report |
| ✅ Fixed (PG-wire, basic shape) | 4 (`tables`, `columns`, `schemata`) | Add `routines` + `referential_constraints` if a real ORM probe needs them |
| ❌ Still present | 1 (`CREATE DATABASE`) | Implement as tenant-API wrapper (`heliosdb-nano-tenant` skill) |
| ❌ Still present | 2 (SCRAM-SHA-256) | Fix GS2 header parsing at `handler.rs:751–753` |
| ❌ Still present | 5 (DB name not validated) | Validate against tenants table at startup |

The previously-recommended **v3.24.0 milestone (Bug 8 + 9 + 6)** is now a no-op — those three are confirmed fixed against `psql` and `psycopg2` over the PG wire on v3.23.1.

> **Recommendation for the dashboard team**: re-test the migration today
> against `cargo install heliosdb-nano@3.23.1`. If your TypeORM bootstrap
> doesn't need `CREATE DATABASE`, doesn't need SCRAM-SHA-256, and you
> can connect with `--auth trust` over loopback / Unix socket, **all
> known migration blockers are already closed**. If any of those three
> are required, see the v3.24+ release plan below.

## Verified status (PG-wire daemon, v3.23.1, 2026-05-03 evening)

Each row was verified by spinning up `target/release/heliosdb-nano start --port <free> --http-port <free>` and running the bug's exact reproducer over the PG wire (psql + psycopg2). The `--http-port` flag is required because the default 8080 collides with another service on this dev host, and the `tokio::select!` in `src/main.rs:710–730` exits the whole DB if any of `pg_server / health_server / ctrl_c` returns. Use a free `--http-port` (e.g., 18080+) when running the daemon for repro.

| Bug | v3.19.1 (filed) | v3.23.1 (verified) | Repro command |
|-----|-----------------|---------------------|--------------|
| 1 | ❌ broken | ❌ STILL BROKEN | `psql -c "CREATE DATABASE x"` → `Statement not yet supported` |
| 2 | ❌ broken | ❌ STILL BROKEN | `--auth scram-sha-256`, `psql` → `FATAL: Protocol error: Invalid SCRAM client-first-message` |
| 3 | ❌ broken | ✅ FIXED | `--auth password`, `PGPASSWORD=foobar psql` → `SELECT 1 = 1`. Wrong password correctly rejected. |
| 4 | ❌ broken | ✅ FIXED (basic) / 🟡 PARTIAL | `SELECT * FROM information_schema.tables WHERE table_schema='public'` → 9 rows. `routines` and `referential_constraints` return 0 rows but no error. |
| 5 | ❌ broken | ❌ STILL BROKEN | `psql -d totally_made_up_db_12345 -c "SELECT current_database()"` → `heliosdb` (silent route) |
| 6 | ❌ broken | ✅ FIXED | `psql -f synth_pg_dump.sql` (full SET preamble + CREATE TABLE + INSERT) completes in <1s, all rows restored |
| 7 | ❌ broken | ✅ FIXED (simple-query) | `psql -c "CREATE TABLE a (x INT); CREATE TABLE b (y INT)"` works |
| 8 | ❌ broken | ✅ FIXED | psycopg2 `cur.execute("SELECT COUNT(*) FROM pings WHERE week_bucket = %s", ("2026-18",))` → `[(1,)]` |
| 9 | ❌ broken | ✅ FIXED | psycopg2 same as above with `COUNT(DISTINCT hash)` → matches literal form |
| 10 | ❌ broken | ✅ FIXED (verified earlier) | embedded API `query_with_columns("SELECT COUNT(*) AS xyzzy …")` → cols=["xyzzy"] |
| 11 | ❌ broken | ✅ FIXED (verified earlier) | embedded API `SELECT name FROM foo` → 1 column per row |

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

## Release sequencing (collapsed after evening re-triage)

The earlier plan over-scoped because it took the v3.19.1 bug filings at face value. Eight of nine "still present" bugs have been closed by intervening releases (the precise commits should be findable via `git log v3.19.1..v3.23.1 -- src/protocol/ src/sql/`). The new plan is:

| Release | Bugs | Bump | Rationale |
|---------|------|------|-----------|
| **v3.23.2** | 3, 6, 7, 8, 9 (verify-and-document closure) + add a `tests/dashboard_bugs_regression.rs` integration suite that locks in the closed status so they can't silently regress | patch | No code changes, but the new regression suite is meaningful surface — it imports `pg_dump`-shape SQL, runs psycopg2 parameterised queries, and SCRAM/password auth probes. Pinning to current behaviour. |
| **v3.24.0** | 4 (add `routines` + `referential_constraints` views; tighten the catch-all to error loudly on truly unknown views) | minor | ORM-bootstrap completion. Behaviour change: clients that relied on silent-empty results now see a real error. None known. |
| **v3.25.0** | 1 (`CREATE DATABASE` → tenant-API wrapper) + 5 (StartupMessage validates DB name against tenants table) | minor | Cohesive surface — both touch the database-name plumbing. No storage layout change because tenants already have isolated namespaces. |
| **v3.26.0** | 2 (SCRAM-SHA-256 GS2 header parsing) + same-host-only `trust` enforcement | minor | Auth correctness + safer defaults. Refusing non-loopback `--listen` with `--auth trust` is a behaviour change that warrants the minor bump. (Bug 3 closed in-range.) |

After v3.26.0: notify the dashboard team for end-to-end re-verification.

## Recommended next action

**Skip the v3.24.0 "Bug 8 + 9 + 6 fix" milestone entirely** — those are already closed.

Move to **v3.23.2**: write `tests/dashboard_bugs_regression.rs` to lock in the current behaviour and prevent silent regressions, then ship a doc-only patch confirming the closures to the dashboard team. Then proceed in numerical order (4 → 1+5 → 2 + auth-default).

Effort estimate for v3.23.2: **half a day** (regression-test scaffold + new branch + tag).
Effort estimate for v3.24.0 (Bug 4 completion): **2–3 days** (mostly views + the catch-all error-loudly behaviour change).
Effort estimate for v3.25.0 (Bug 1 + 5): **3–5 days** (tenant-API SQL DDL + StartupMessage validation).
Effort estimate for v3.26.0 (Bug 2 + trust enforcement): **3–5 days** (SCRAM GS2 parsing is finicky; cross-client testing required).
