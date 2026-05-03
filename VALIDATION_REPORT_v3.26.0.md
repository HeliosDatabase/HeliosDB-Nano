---
release: v3.26.0
branch: feat/scram-gs2-trust-loopback
date: 2026-05-03
methodology: .claude/skills/heliosdb-nano-merge-validation/SKILL.md (8 phases)
---

# v3.26.0 — SCRAM GS2 header parsing + same-host-only `trust` (Bug 2)

## Summary

Closes Bug 2 from the dashboard-migration triage and tightens auth defaults:

- **Bug 2**: SCRAM-SHA-256 `client-first-message` parser at `src/protocol/postgres/handler.rs:768-777` was splitting on commas and indexing `parts[1]` as the username. Real clients (libpq, asyncpg, node-postgres, JDBC, sqlx, psycopg2) send `n,,n=user,r=nonce` per RFC 5802 — the leading `n,,` is the GS2 header. The old parser was effectively broken for every compliant client.
- **Same-host-only `trust`**: `PgServer::new` and `PgServer::with_auth_manager` now refuse to construct when `auth_method = Trust` and `address.ip()` is non-loopback. Production deployments with `--listen 0.0.0.0` must use a non-trust auth method.

## Phase 1 — Branch + scope

Branched from `main` at `66a0654` (v3.25.0) → `feat/scram-gs2-trust-loopback`.

Scope: 2 files in `src/protocol/postgres/` (`auth.rs`, `handler.rs`, `server.rs`); +~110 LOC; one new public function (`parse_scram_client_first`) + one new safety gate (`enforce_trust_loopback_only`).

## Phase 2 — Targeted unit tests (`tests/scram_gs2_and_trust_loopback.rs`)

13 tests covering both surfaces:

| # | Test | Pre-impl | Post-impl |
|---|------|----------|-----------|
| 1 | `scram_parser_handles_libpq_gs2_header` | ❌ FAIL (no parser fn) | ✅ PASS |
| 2 | `scram_parser_handles_authzid` | ❌ FAIL | ✅ PASS |
| 3 | `scram_parser_handles_y_channel_binding_flag` | ❌ FAIL | ✅ PASS |
| 4 | `scram_parser_rejects_truncated_message` | ❌ FAIL | ✅ PASS |
| 5 | `scram_parser_rejects_missing_username` | ❌ FAIL | ✅ PASS |
| 6 | `scram_parser_rejects_missing_nonce` | ❌ FAIL | ✅ PASS |
| 7 | `trust_auth_on_loopback_is_allowed` | ✅ pass¹ | ✅ PASS |
| 8 | `trust_auth_on_ipv6_loopback_is_allowed` | ✅ pass¹ | ✅ PASS |
| 9 | `trust_auth_on_unspecified_address_is_refused` | ❌ FAIL | ✅ PASS |
| 10 | `trust_auth_on_public_address_is_refused` | ❌ FAIL | ✅ PASS |
| 11 | `password_auth_on_public_address_is_allowed` | ✅ pass | ✅ PASS |
| 12 | `scram_auth_on_public_address_is_allowed` | ✅ pass | ✅ PASS |
| 13 | `with_auth_manager_also_enforces_trust_loopback` | ❌ FAIL | ✅ PASS |

¹ Tests 7-8 passed pre-impl because the trust-loopback gate didn't exist yet — *every* trust deployment was permitted.

```
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Phase 3 — Implementation

### `src/protocol/postgres/auth.rs`

- **New `parse_scram_client_first(msg) -> Result<(String, String)>`**: properly walks the GS2 header per RFC 5802. Uses `splitn(3, ',')` to skip the cbind-flag and authzid slots, then scans the bare body for `n=` and `r=` tokens (order-independent within the bare body).
- **`parse_scram_client_first_for_test`**: hidden re-export for `tests/`. Same body, distinct name to keep the public surface tight while letting integration tests exercise the parser without a full PG-wire round-trip.

### `src/protocol/postgres/handler.rs`

- `handle_scram_authentication`: the broken parse-and-index block is replaced with a single call to `auth::parse_scram_client_first(&client_first)?`. Comment block updated to document the RFC.

### `src/protocol/postgres/server.rs`

- New private fn `enforce_trust_loopback_only(config)`: returns `Err(Error::authentication(...))` when `auth_method = Trust && !address.ip().is_loopback()`.
- `PgServer::new` calls the gate before any other side-effects.
- `PgServer::with_auth_manager` applies the same check using `auth_manager.method()` (the user might have built the AuthManager differently from `config.auth_method`).

## Phase 4 — Targeted feature bench

**Skipped.** Both surfaces are once-per-connection auth code, dwarfed by TCP / TLS handshake costs. Parser change is from "split-on-comma + index" to "split-on-comma + scan" — same big-O.

## Phase 5 — Cross-feature regression

| Suite | Tests | Result |
|-------|-------|--------|
| `cargo test --lib --release` | 1758 | ✅ all pass |
| `tests/scram_gs2_and_trust_loopback.rs` (new) | 13 | ✅ all pass |
| `tests/postgres_scram_auth_tests` | 25/26 | ✅ all but one pre-existing flake |
| `tests/postgres_extended_protocol_tests` | (catalog-touching) | ✅ pass |
| `tests/create_database_and_dbname_validation.rs` (v3.25.0) | 8 | ✅ all pass |
| `tests/information_schema_completion.rs` (v3.24.0) | 9 | ✅ all pass |

**Pre-existing flake (not a regression)**:
`postgres_scram_auth_tests::test_auth_manager_timing_attack_resistance` asserts `time_existing_user.as_micros() > 0 && time_nonexistent_user.as_micros() > 0`. On fast hardware the operation completes in < 1µs, so the wall-clock measurement is 0. This was failing on `main` before this branch (verified during v3.24.0 validation; documented in `VALIDATION_REPORT_v3.24.0.md`). It is unrelated to the SCRAM parser change.

## Phase 6 — Head-to-head OLTP

**Skipped.** Auth runs once per connection. Steady-state OLTP is unaffected.

## Phase 7 — Risk matrix & merge recommendation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Parser is now stricter — clients that previously misbehaved silently now return well-formed protocol errors | Low | Low | The previous parser was incorrect for *every* compliant client, so any client connecting today via SCRAM was either using non-SCRAM auth or hitting a code path that bypassed the parser. The new parser is RFC-correct. |
| Production deployments with `--listen 0.0.0.0 --auth trust` no longer start | Medium | High | Documented behaviour change. Operators get a clear error message naming the safe alternatives (password, scram-sha-256). The change is intentional — silent acceptance on a public interface is a footgun. |
| Channel binding (`p=`) clients fail | Low | Low | Parser accepts `p=...` as a valid GS2 cbind-flag (matches RFC). Channel binding *enforcement* is a follow-up; today the parser tolerates the flag. |
| Auth-manager method != config.auth_method | Low | Low | `with_auth_manager` checks `auth_manager.method()` rather than `config.auth_method`. If someone constructs `AuthManager::new(Trust)` and passes it with a Trust-config, both produce the same gate result. If they mismatch, the AuthManager method wins (it's the actual runtime behaviour). |

**Recommendation: merge.** Targeted tests pass, lib tests are clean, behaviour change is well-scoped and well-documented. After this release the dashboard-migration batch is complete (Bugs 1-9 closed, Bugs 10-11 confirmed already-closed in earlier releases).

## Phase 8 — Release plan

- `Cargo.toml`: bump `version = "3.26.0"`.
- `CHANGELOG.md`: add `## [3.26.0] - 2026-05-03` section with the Bug 2 closure note + same-host-only-trust behaviour change.
- `git tag v3.26.0 && git push --tags` → CI publishes to crates.io.
- Notify dashboard team for batched re-verification of all 11 originally-filed bugs.
