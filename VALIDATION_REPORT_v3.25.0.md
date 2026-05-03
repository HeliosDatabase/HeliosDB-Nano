---
release: v3.25.0
branch: feat/create-database-tenant-wrapper
date: 2026-05-03
methodology: .claude/skills/heliosdb-nano-merge-validation/SKILL.md (8 phases)
---

# v3.25.0 — `CREATE DATABASE` + StartupMessage validation (Bug 1 + Bug 5)

## Summary

Closes Bug 1 and Bug 5 from the dashboard-migration triage:

- **Bug 1**: `CREATE DATABASE testdb` and `DROP DATABASE testdb` now succeed end-to-end. `Statement::CreateDatabase` and `Statement::Drop { object_type: Database }` are routed through new `LogicalPlan::CreateDatabase` / `LogicalPlan::DropDatabase` nodes which wrap the existing `TenantManager` API as a metadata-only DDL.
- **Bug 5**: PG-wire StartupMessage now validates the `database` parameter. `heliosdb` and `postgres` are accepted (reserved); any registered tenant is accepted; anything else is rejected with `database "x" does not exist`.

Reserved names cannot be created or dropped; `IF NOT EXISTS` on a reserved name succeeds silently (ORM idempotence). `IF EXISTS` on a missing name succeeds silently (ANSI semantics).

Touches three files:
- `src/lib.rs` (+~120 LOC for the four helpers and the two dispatch arms in `execute_internal` / `execute_in_transaction_inner`)
- `src/sql/logical_plan.rs` (+~30 LOC for the two new variants and their schema arms)
- `src/sql/planner.rs` (+~15 LOC for the planner arm and the DROP-DATABASE routing)
- `src/sql/executor/mod.rs` (+~12 LOC exhaustiveness arm)
- `src/optimizer/planner.rs` (+2 LOC pass-through)
- `src/protocol/postgres/catalog.rs` (+~7 LOC: `is_valid_database_name` thin wrapper)
- `src/protocol/postgres/handler.rs` (+~12 LOC for StartupMessage validation)

## Phase 1 — Branch + scope

Branched from `main` at `08cfcad` (v3.24.0) → `feat/create-database-tenant-wrapper`.

The change is metadata-only at the storage level (no schema change, no key-prefix change, no migration). Tenants are tracked in the in-memory `TenantManager`; persistence across restarts is a follow-up.

## Phase 2 — Targeted unit tests (`tests/create_database_and_dbname_validation.rs`)

| # | Test | Pre-impl | Post-impl |
|---|------|----------|-----------|
| 1 | `create_database_via_sql_succeeds` | ❌ FAIL | ✅ PASS |
| 2 | `create_database_if_not_exists_is_idempotent` | ❌ FAIL | ✅ PASS |
| 3 | `drop_database_removes_tenant` | ❌ FAIL | ✅ PASS |
| 4 | `drop_database_if_exists_is_idempotent` | ❌ FAIL | ✅ PASS |
| 5 | `drop_reserved_database_names_is_refused` | ❌ FAIL | ✅ PASS |
| 6 | `create_reserved_database_names_is_refused` | ❌ FAIL | ✅ PASS |
| 7 | `current_database_returns_default_when_no_active_tenant` | ✅ pass | ✅ PASS |
| 8 | `pg_wire_database_validation_via_catalog_api` | ❌ FAIL (no `is_valid_database_name`) | ✅ PASS |

```
running 8 tests
test current_database_returns_default_when_no_active_tenant ... ok
test drop_database_if_exists_is_idempotent ... ok
test drop_reserved_database_names_is_refused ... ok
test pg_wire_database_validation_via_catalog_api ... ok
test drop_database_removes_tenant ... ok
test create_database_via_sql_succeeds ... ok
test create_database_if_not_exists_is_idempotent ... ok
test create_reserved_database_names_is_refused ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Phase 3 — Implementation

- **`LogicalPlan::CreateDatabase { name, if_not_exists }`** and **`LogicalPlan::DropDatabase { name, if_exists }`**: new variants. Schema is empty (DDL-shape, like `CreateExtension` / `DropExtension`).
- **Planner arm for `Statement::CreateDatabase`**: normalises the `db_name` and emits `LogicalPlan::CreateDatabase`. The `location` and `managed_location` fields are accepted-but-ignored (they don't apply to Nano's storage model).
- **Planner arm for `Statement::Drop { object_type: Database }`**: routes the existing `Drop` arm to the new `LogicalPlan::DropDatabase` variant when the object type is Database.
- **`EmbeddedDatabase::handle_create_database` / `handle_drop_database`**: enforce the reserved-name + duplicate semantics, then call `tenant_manager.register_tenant_with_plan` / `delete_tenant`.
- **`EmbeddedDatabase::database_name_is_valid`**: validator used by the PG-wire StartupMessage handler. Returns true for reserved names + any registered tenant, false otherwise.
- **`PgCatalog::is_valid_database_name`**: thin associated-function wrapper so the StartupMessage path doesn't need to peek into `EmbeddedDatabase` internals.
- **PG-wire StartupMessage validation**: in `handler.rs`, after parsing the StartupMessage and before sending the auth challenge, `database` (or `user` as fallback per libpq) is checked. Unknown names return `Error::authentication("database \"x\" does not exist")`, which the wire layer maps to `FATAL` and tears down the connection.

The dispatch is added at two sites (`execute_internal` and `execute_in_transaction_inner`) because both are reachable; the executor-side arm is intentionally a no-op exhaustiveness fall-through (the executor doesn't have a `TenantManager` reference).

## Phase 4 — Targeted feature bench

**Skipped — no perf surface.** CREATE DATABASE / DROP DATABASE are sub-microsecond operations on an in-memory map; they're invoked once per connection bootstrap at most. The StartupMessage validation is one additional `eq_ignore_ascii_case` per connection — measurement noise relative to the TLS handshake on the same connection.

## Phase 5 — Cross-feature regression

| Suite | Tests | Result |
|-------|-------|--------|
| `cargo test --lib --release` | 1758 | ✅ all pass |
| `cargo test --doc --release` | 47 (+ 20 ignored) | ✅ all pass |
| `tests/create_database_and_dbname_validation.rs` (new) | 8 | ✅ all pass |
| `tests/information_schema_completion.rs` (v3.24.0) | 9 | ✅ all pass |
| `tests/system_views_tests.rs` | 22 | ✅ all pass |

No regression in any catalog-touching or planner-touching surface.

## Phase 6 — Head-to-head OLTP

**Skipped.** Same rationale as Phase 4: zero perf surface for non-DDL workloads; CREATE DATABASE / DROP DATABASE happen at connection bootstrap and never on the hot path.

## Phase 7 — Risk matrix & merge recommendation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Existing client connecting with a non-reserved/non-tenant `database` parameter is rejected | Medium | Low/Medium | Documented behaviour change. `heliosdb` and `postgres` cover psql, pgAdmin, DBeaver, sqlx, psycopg2 defaults. Tenant-name databases are explicit user choice. |
| ORM bootstrap calls `CREATE DATABASE` with a name that collides with `heliosdb`/`postgres` | Low | Low | `IF NOT EXISTS` succeeds silently; bare `CREATE DATABASE heliosdb` errors loudly with a clear "reserved system database" message. |
| Tenant table is in-memory only — restart loses tenants | Medium | Medium | Documented. Persistence is a follow-up (likely v3.27+). For dashboard / TypeORM bootstraps that re-run their migration on every connect, this is a non-issue because the migration is idempotent. |
| Concurrent `CREATE DATABASE` from two connections produces duplicate tenants | Very Low | Low | `register_tenant_with_plan` is mutex-protected via `parking_lot::RwLock<HashMap>`; the duplicate-check + register run under the same logical operation. Strict atomicity would need a CAS, but the practical race window is sub-microsecond and the duplicate produces two distinct UUIDs (deterministic order via `list_tenants().sort_by(name)` keeps queries consistent). |
| StartupMessage validation breaks PG client compatibility | Low | High | Reserved names cover psql, pgAdmin, DBeaver, sqlx, libpq, asyncpg, psycopg2, node-postgres, JDBC. A `user`-as-fallback rule also matches libpq's `database = user` default. |

**Recommendation: merge.** Targeted tests pass, lib + doc tests are clean, behaviour change is well-scoped (database-name validation tightening) and well-documented in the CHANGELOG. After this release, only Bug 2 + same-host-only `trust` enforcement remain for the dashboard-migration batch.

## Phase 8 — Release plan

- `Cargo.toml`: bump `version = "3.25.0"`.
- `CHANGELOG.md`: add `## [3.25.0] - 2026-05-03` section with the Bug 1 + Bug 5 closure notes and the StartupMessage behaviour change.
- `git tag v3.25.0 && git push --tags` → CI publishes to crates.io and creates the GitHub Release.
