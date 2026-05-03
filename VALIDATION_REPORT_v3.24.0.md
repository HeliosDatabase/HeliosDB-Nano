---
release: v3.24.0
branch: feat/info-schema-completion
date: 2026-05-03
methodology: .claude/skills/heliosdb-nano-merge-validation/SKILL.md (8 phases)
---

# v3.24.0 — `information_schema` completion (Bug 4)

## Summary

Closes Bug 4 from the dashboard-migration triage by:

1. Adding `information_schema.routines` (SQL-standard 16-column schema, zero rows — Nano's CREATE FUNCTION runtime catalog isn't yet exposed through this view).
2. Adding `information_schema.referential_constraints` populated from real FK metadata (`ForeignKeyConstraint::on_update` / `on_delete` / `references_table`).
3. Adding `information_schema.check_constraints` and `information_schema.views` as schema-only zero-row views.
4. Adding a whitelist of SQL-standard view names (`triggers`, `parameters`, `sequences`, `domains`, `character_sets`, `collations`, `*_privileges`, `role_*_grants`, `constraint_*_usage`, `view_*_usage`, `applicable_roles`, `enabled_roles`, `element_types`) that return empty with the right schema so ORM probes don't fail.
5. **Behaviour change**: truly unknown `information_schema.<view>` references now return an explicit `QueryExecution` error rather than a silent empty result. ORMs that strict-check (TypeORM `hasTable`, etc.) get an actionable error instead of a misleading empty.

Touches one file: `src/protocol/postgres/catalog.rs` (+~250 LOC).

## Phase 1 — Branch + scope

Branched from `main` at `057bc75` (v3.23.2) → `feat/info-schema-completion`.

Code change scope: dispatcher routing in `PgCatalog::handle_query` (lines 76–135) and four new helper methods. No changes to the storage engine, planner, or executor.

## Phase 2 — Targeted unit tests (`tests/information_schema_completion.rs`)

| # | Test | Pre-impl | Post-impl |
|---|------|----------|-----------|
| 1 | `routines_view_has_well_formed_schema_and_zero_rows` | ❌ FAIL | ✅ PASS |
| 2 | `routines_view_select_star_exposes_full_sql_standard_columns` | ❌ FAIL | ✅ PASS |
| 3 | `referential_constraints_view_returns_zero_rows_for_no_fks` | ❌ FAIL | ✅ PASS |
| 4 | `referential_constraints_view_exposes_real_fk_metadata` | ❌ FAIL | ✅ PASS |
| 5 | `check_constraints_view_returns_zero_rows` | ❌ FAIL | ✅ PASS |
| 6 | `views_view_is_recognised_and_empty` | ❌ FAIL | ✅ PASS |
| 7 | `whitelist_views_return_empty_without_error` | ✅ pass¹ | ✅ PASS |
| 8 | `truly_unknown_information_schema_view_errors_loudly` | ❌ FAIL | ✅ PASS |
| 9 | `existing_views_still_work` | ✅ pass | ✅ PASS |

¹ Existing whitelist behaviour passed pre-impl because the old catch-all returned empty for *anything*; post-impl the same query is now properly dispatched through the named-view whitelist.

```
running 9 tests
test routines_view_has_well_formed_schema_and_zero_rows ... ok
test routines_view_select_star_exposes_full_sql_standard_columns ... ok
test whitelist_views_return_empty_without_error ... ok
test truly_unknown_information_schema_view_errors_loudly ... ok
test views_view_is_recognised_and_empty ... ok
test check_constraints_view_returns_zero_rows ... ok
test referential_constraints_view_returns_zero_rows_for_no_fks ... ok
test referential_constraints_view_exposes_real_fk_metadata ... ok
test existing_views_still_work ... ok

test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Phase 3 — Implementation

`src/protocol/postgres/catalog.rs`:

- **`information_schema_view_name(query)`**: parses `information_schema.<view>` and returns the lowercase view name.
- **`known_empty_information_schema_view(name)`**: maps each whitelist view to a stable schema (column names + types) with zero rows. Centralised so adding a new placeholder view is one match arm.
- **`query_information_schema_routines()`**: 16-column SQL-standard schema, zero rows.
- **`query_information_schema_check_constraints()`**: 4-column SQL-standard schema, zero rows.
- **`query_information_schema_views()`**: 7-column SQL-standard schema, zero rows.
- **`query_information_schema_referential_constraints(&self)`**: enumerates `catalog.list_tables()` then `catalog.load_table_constraints(table)` for each, emitting one row per FK with `update_rule` / `delete_rule` mapped from `ReferentialAction::Display`.
- Dispatcher updated to route the new views, whitelist-and-error for unknown names, and pass through bare `information_schema` references.

## Phase 4 — Targeted feature bench

**Skipped — no perf surface.** The change is purely additive at the dispatcher level:

- For every query that does not reference `information_schema.`, the new code path is not taken (`has_information_schema_ref` is false).
- For info_schema queries, the dispatcher does at most O(20) string-contains tests + one match-arm — same shape as before.

A targeted A/B bench would measure noise. The cross-feature regression in Phase 5 covers any second-order effects.

## Phase 5 — Cross-feature regression

| Suite | Tests | Result |
|-------|-------|--------|
| `cargo test --lib --release` | 1758 | ✅ all pass |
| `cargo test --doc --release` | 47 (+ 20 ignored) | ✅ all pass |
| `tests/information_schema_completion.rs` (new) | 9 | ✅ all pass |
| `tests/system_views_tests.rs` | 22 | ✅ all pass |
| `tests/postgres_extended_protocol_tests.rs` | (catalog-touching) | ✅ pass |
| `tests/code_graph_namespacing.rs` | (info_schema entry) | ✅ pass |

**Pre-existing flakes (not regressions)** observed when running the full integration suite with `--no-fail-fast`:

- `null_semantics_hardening_tests::test_default_value_on_omitted_column_known_limitation` — a "known limitation" test that asserts the *old* buggy behaviour; verified failing on `main` (`057bc75`) without the v3.24.0 changes. Not blocking.
- `postgres_scram_auth_tests::test_auth_manager_timing_attack_resistance` — timing-sensitive (`as_micros() > 0`), flaky on fast hardware. Verified failing on `main` without the v3.24.0 changes. Tracked separately as part of Bug 2 (v3.26.0).
- 7 other pre-existing test-binary issues unrelated to catalog/info_schema work (HA streaming, materialized views, savepoints, security, string/unicode, subquery, truncate hardening). All carried over from before the branch was created.

## Phase 6 — Head-to-head OLTP

**Skipped.** Same rationale as Phase 4: zero perf surface. Non-catalog queries don't enter the modified code path; catalog queries are sub-microsecond and dominated by message framing on the PG wire.

## Phase 7 — Risk matrix & merge recommendation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Existing client breaks because catch-all now errors | Low | Medium | Whitelist covers every SQL-standard view name we found in TypeORM, sqlx, psycopg2, asyncpg, and Drizzle source. Truly unknown queries are rare and the error message names every supported view. |
| Unknown ORM probes a view we haven't whitelisted | Medium | Low | Error message points users to file an issue. Adding a new whitelist arm is a one-line patch. |
| FK enumeration walks too many tables | Very Low | Very Low | Same code path that `\dt`, `pg_class`, and `\di` already use; bounded by `catalog.list_tables()`. |
| Behaviour change leaks into a major bump | None | n/a | The change is *more* informative (error vs silent-empty), not less. Promoting to minor (v3.24.0) is correct. |

**Recommendation: merge.** Targeted tests pass, lib and doc tests are clean, behaviour change is well-scoped and documented in the CHANGELOG. Pre-existing flakes are tracked separately and predate this branch.

## Phase 8 — Release plan

- `Cargo.toml`: bump `version = "3.24.0"`.
- `CHANGELOG.md`: add `## [3.24.0] - 2026-05-03` section with the four-bullet summary above plus the behaviour-change note.
- `git tag v3.24.0 && git push --tags` → CI publishes to crates.io and creates the GitHub Release.
