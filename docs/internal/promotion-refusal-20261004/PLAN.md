# Durable source plan — truthful SQL switchover refusal

Task: bounded repair of the reachable SQL switchover placeholder.
Worktree: `/home/gpc/HDB/worktrees/nano-promotion-refusal-20261004`
Branch: `fix/nano-promotion-refusal-20261004`
Base: `56b19dc42ce867a2314878543b3b156b6f92980b`
Revision: R2 — scoped to SWITCHOVER CHECK, trimmed error, handoff re-shaped.

## 1. Defect

`src/sql/executor/ddl.rs::handle_switchover` (reachable from the text family via
`EmbeddedDatabase::execute` → `try_parse_ha_command` → `LogicalPlan::Switchover`
→ `Executor::plan_to_operator`) resolves the target, validates caller role and
standby presence, and then returned a **success-shaped status**
(`StatusMessageOperator`) whose text says `Switchover to node … initiated` while
its own comment admits the runtime coordination is unimplemented.

`handle_switchover_check` is the read-only sibling: it populated health fields
but reported `can_proceed = true` for a healthy connected standby, which
misrepresents runtime availability — the check says "you may proceed" when no
runtime promotion exists.

`Error::switchover` → `Error::Generic("Switchover error: …")` (Display = text).

## 2. Change

1. `src/sql/executor/ddl.rs::handle_switchover` — replace the placeholder success
   with a short, actionable `Err(Error::switchover(...))`: runtime
   switchover/promotion unsupported, durable old-primary fencing not
   implemented, no role changed. Keep the three existing validations
   (target resolution, primary-only, target standby present). The long-form
   requirements belong to the operator doc, not the user-facing error or the
   source comment.
2. `src/sql/executor/ddl.rs::handle_switchover_check` — preserve the schema and
   every health field (`target_healthy`, `target_lsn`, `primary_lsn`,
   `lag_bytes`, `warnings`), but set `can_proceed = false` for **every** call and
   seed `blockers` with `"Runtime switchover/promotion is unsupported"`, even for
   a valid connected standby. Existing role/target blockers are still appended.
3. `tests/promotion_refusal_tests.rs` (new, **standalone integration target** so
   it owns its own process and therefore its own `ha_state()` singleton — the
   reason `txn_replication_sync.rs` is separate; do not move this into
   `tests/ha_tests/`). Reach the real SQL handlers:
   - in-memory DB, create table, insert a row;
   - arm HA: `ha_state().set_config(role = Primary)`, register one standby with
     a known UUID — this is the "private fixture"; it deliberately satisfies the
     primary + standby preconditions so the test does **not** pass simply
     because standby prevalidation failed;
   - execute `SWITCHOVER TO '<uuid>'`; assert `Err` whose message names
     `unsupported`, `fencing` and `no role was changed`;
   - execute `SWITCHOVER CHECK '<uuid>'` as real SQL; assert it runs (`Ok(1)`,
     i.e. not a parse/validation failure) and is side-effect-free; additionally
     re-parse the identical SQL with `Parser::parse_switchover_check_sql`, form
     `LogicalPlan::SwitchoverCheck`, run it through the public `sql::Executor`
     (`Executor::with_storage(&db.storage)`) and assert the preserved seven-column
     schema (names + types), exactly one row, `can_proceed = Boolean(false)`,
     `target_healthy = Boolean(true)`, and `blockers` containing
     `"Runtime switchover/promotion is unsupported"`;
   - assert HA role and read-only are unchanged (still Primary / not read-only);
   - assert actual rows and catalog (`information_schema.tables`) are unchanged;
   - capture all observations before restoring global HA state
     (`remove_standby`, `set_config(default)`), then assert, so a panic does not
     leak Primary/standby into the process.
   No fixed port; no network; no filesystem path; one test, no concurrency
   against the process-global HA singleton.

4. `docs/guides/replication-history-guard.md` — add the operator-facing
   "promotion is refused" contract: native catch-up repair is not promotion; raw
   physical resync is NON-SERVING; retained standby binding refuses implicit
   primary reopen; what a safe promotion would require; the fact that
   `RoleManager` (in-memory role mutation), `WalApplicator::promote` (stops
   application/log only) and the legacy failover path (continues after failed
   election/best-effort fencing) are not a safe promotion implementation.

Not touched: root `sql/` (stale HeliosDB-Lite-era duplicate, excluded from the
crate and never compiled; `pub mod sql` resolves to `src/sql`), the other
worktree, any guard, write routing, or automatic takeover. No production API is
expanded; the CHECK row is observed through the already-public
`sql::Executor`/`Parser` surface, not by adding query-path routing.

## 3. Unrun checks (heavy gates remain root-owned; not executed here)

- `cargo fmt --all -- --check` (changed files clean)
- `cargo test --lib`
- `cargo test --test promotion_refusal_tests`
- `cargo test --test txn_replication_sync`
- full suite + doc + clippy new-findings comparison per `docs/GATES.md`, run in
  the root fleet schedule (frozen baseline worktree, never `git stash`).

## 4. Handoff

Changed-file inventory, the frozen-baseline clippy rule and exact unrun commands
are in `HANDOFF.md` beside this plan. No commit, push, release or test run
performed in this task.
