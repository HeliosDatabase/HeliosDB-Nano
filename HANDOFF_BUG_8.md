---
branch: fix/extended-query-rowdescription
parent-tag: v3.23.1
session-handoff: 2026-05-03
target-release: v3.24.0
covers: Bug 8 (parameterised SELECT crashes node-pg) + Bug 9 (COUNT(DISTINCT) with $param returns 0) + Bug 6 (pg_dump restore stalls — likely shares root)
---

# Bug 8 / 9 / 6 — investigation handoff

## State at handoff

- Branch `fix/extended-query-rowdescription` exists, branched from `main` at v3.23.1.
- Phase 1 (branch + implement) — branch created.
- Phase 2 (failing tests) — `tests/extended_query_param_select.rs` written; three `#[ignore]`'d tokio-postgres tests that exercise the failing extended-query path. The `#[ignore]` should be removed once the daemon-startup race is resolved (see Pitfalls below).
- Phase 3+ — pending.

## What I know about the bug (from explorers + code reading)

### The hypothesis I came in with

Per the bug report and explorer scans: `parse_extended` in `src/protocol/postgres/handler_extended.rs:60–66` calls `derive_result_schema(statement)`. If that errors, it falls back to `synthesise_schema_from_ast` at `src/protocol/postgres/handler_extended.rs:534–550`. The synthesis path was suspected of producing column descriptors with empty names, leading to a malformed RowDescription, which crashes node-pg's parser at `pg-pool/index.js:45` with `Cannot read properties of undefined (reading 'name')`.

### What I verified by reading the code

- The synthesis path **does** populate names: `crate::Column::new(name, DataType::Text)` where `name` comes from `expr_column_label(expr).unwrap_or_else(|| format!("column{}", i + 1))` — so even unnamed exprs get `column1`, `column2`, etc. Names are never empty.
- The Describe handler at `src/protocol/postgres/handler_extended.rs:340–369` correctly translates `Schema.columns[].name` into `FieldDescription.name`.
- The wire-format encoder at `src/protocol/postgres/messages.rs:491–511` correctly writes `name` as a cstring then 18 bytes of fixed fields per column. Length math (`name.len() + 1 + 18`) checks out.
- `datatype_to_oid` at `src/protocol/postgres/handler.rs:1347–1366` falls back to `705` for unknown types — node-pg accepts that (it treats it as text).

So **the hypothesis I started with does not localise the bug.** The synthesis path looks fine. The Describe handler looks fine. The serialiser looks fine. Either:
- The bug is in `derive_result_schema` for parameterised SELECT — it succeeds with a degenerate schema, and the synthesis fallback never fires.
- The bug is in a code path I haven't read (Bind / Execute / DataRow encoding for parameterised plans), and what node-pg really chokes on is a length-mismatched DataRow, not a malformed RowDescription.
- The bug is in how the planner builds the schema for `SELECT COUNT(*)` when the AST contains `$1` — `Aggregate` plan node's output schema may have an unnamed column that propagates through.

### Where to start next session

1. **Get a deterministic repro running.** The tests in `tests/extended_query_param_select.rs` are scaffolded with `#[ignore]` for now because the in-process server pattern (per `tests/server_mode_integration_test.rs`) has historically had issues. Three options:
   - In-process pattern: `PgServer::new() + .serve()` spawned via `tokio::spawn`. Some existing tests are `#[ignore]`'d for "stack overflow issue" — investigate if that still applies.
   - External daemon: spawn `target/release/heliosdb-nano start --port <free> --auth trust`, connect via tokio-postgres. Earlier in the session the daemon kept exiting cleanly when invoked via the Bash tool; needs `setsid` + careful PID-file handling, OR run via `gh run` somehow.
   - Capture the wire bytes: tcpdump / wireshark on the connection while psql runs the query. Diff what we send vs what real PostgreSQL sends for the same query.

2. **Rule out the planner.** Run the unit-level test:
   ```rust
   let db = EmbeddedDatabase::new_in_memory()?;
   db.execute("CREATE TABLE pings (week_bucket TEXT)")?;
   let planner = Planner::with_catalog(&db.storage.catalog());
   let plan = planner.statement_to_plan(parse_one("SELECT COUNT(*) FROM pings WHERE week_bucket = $1")?)?;
   let schema = plan.schema();
   for col in &schema.columns {
       println!("col: name={:?} type={:?}", col.name, col.data_type);
   }
   ```
   If any `name` is empty, that's the source. If the names look OK, the bug is downstream.

3. **Read Bind + Execute paths.** `handle_bind_extended` at `src/protocol/postgres/handler_extended.rs:97+` and the Execute handler are after the Describe step. The crash is on `RowDescription`, but it could be that Describe sends OK and then the Execute response is malformed in a way node-pg attributes to RowDescription.

4. **Compare to simple-query path.** The same `SELECT COUNT(*) FROM pings WHERE week_bucket = '2026-18'` works on the simple-query path (Bug 9 confirms this). The simple-query handler is at `handler.rs:854,890`. Diff the schema-derivation between simple-query and extended-query — the divergence is the bug.

## Why Bug 6 likely shares this root

`pg_dump` restore stalls when `psql -f dump.sql` runs. `psql -f` uses **simple-query** for most statements but switches to **extended-query** when `\copy` directives (or specific SET forms) appear. If the extended-query path has a stuck or malformed response on certain statements (e.g., `SET row_security = off`), `psql` can spin waiting for a `ReadyForQuery` that never arrives. Confirm by running `pg_dump | psql` against a v3.23.1 server-on-port and capturing the wire trace; identify which statement hangs.

## Methodology checklist (per heliosdb-nano-merge-validation)

| Phase | Status |
|-------|--------|
| 1. Branch + implement | ✅ branch created, no code changes yet |
| 2. Targeted unit tests | ✅ scaffolded; `#[ignore]`'d pending repro pipeline |
| 3. Integration regression | pending — run after fix lands |
| 4. Targeted feature bench | pending — `benches/extended_query_bench.rs` design in triage doc |
| 5. Cross-feature regression | pending — `art_index_bench`, `branch_performance`, `vector_search_bench` |
| 6. Head-to-head OLTP vs main | pending — `examples/oltp_smoke.rs` already exists, just rerun on this branch |
| 7. Validation report | pending — write `EXTENDED_QUERY_REPORT.md` covering Bug 8 + 9 + 6 |
| 8. Release | pending — v3.24.0 (minor; behaviour change for prepared-statement clients) |

## Pitfalls (lessons from this session)

- **Daemon detachment is fragile under the Bash tool's background mode.** `nohup`, `setsid`, `disown` — none reliably keep the daemon alive when the parent shell exits. The in-process `PgServer` pattern from `tests/server_mode_integration_test.rs` is the proven path for tokio-postgres tests; use that, but check for the "stack overflow issue" the existing test comments mention.
- **Don't trust the bug report's location guess without verifying.** The dashboard team's `pg-pool/index.js:45` reference points at node-pg's parser; the actual server-side defect may be many layers earlier (planner, schema, encoder, length-math) before the bytes that confuse node-pg are produced.
- **Bug 9 is probably trivial once Bug 8 is fixed.** The `#[ignore]` test for Bug 9 should pass automatically; verify before declaring it a separate fix.
- **Bug 6 verification needs a real `pg_dump` output as a fixture.** Generate one against PG 15 with the dashboard's schema, commit it as `tests/fixtures/pg_dump_smoke.sql`, then run `psql -f` against Nano in the test harness.

## See also

- `BUGS_DASHBOARD_MIGRATION_TRIAGE.md` — full sequencing.
- `.claude/skills/heliosdb-nano-merge-validation/SKILL.md` — eight-phase methodology.
- `tests/extended_query_param_select.rs` — three `#[ignore]`'d failing tests on this branch.
- `src/protocol/postgres/handler_extended.rs` — primary suspect file.
- `src/protocol/postgres/handler.rs:854,890` — simple-query RowDescription path (works today; useful diff target).
- `tests/server_mode_integration_test.rs` — in-process PG-wire test pattern with tokio-postgres.
