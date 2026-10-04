# Handoff — truthful SQL switchover refusal / CHECK status

Branch: `fix/nano-promotion-refusal-20261004`
Base: `56b19dc42ce867a2314878543b3b156b6f92980b`
Status: implemented, **unbuilt / untested** (root-owned heavy gates not run).
No commit, push, release or issue change was made.
Revision: R2 — CHECK scope added, error/comment trimmed, handoff re-shaped.

## 1. Changed-file inventory

Modified (reviewable source unit):

| File | Change |
|---|---|
| `src/sql/executor/ddl.rs` | `handle_switchover` returns a short truthful `Err(Error::switchover(...))` instead of a success-shaped `StatusMessageOperator`; preconditions unchanged. `handle_switchover_check` keeps its schema and every health field but now sets `can_proceed = false` for all calls and seeds `blockers` with `"Runtime switchover/promotion is unsupported"`; the existing role/target blockers still append. |
| `docs/guides/replication-history-guard.md` | Operator section "Promotion is refused" stating the supported contract. |

Added:

| File | Purpose |
|---|---|
| `tests/promotion_refusal_tests.rs` | Standalone integration target (own process ⇒ own `ha_state()` singleton). One test drives real SQL `SWITCHOVER TO '<uuid>'` and `SWITCHOVER CHECK '<uuid>'` through the real handlers with a primary + registered-standby fixture. The CHECK row is additionally observed through the public plan-level API (`Parser::parse_switchover_check_sql` → `LogicalPlan::SwitchoverCheck` → `sql::Executor::execute_with_schema`), so the field values are asserted, not just the affected-row count. |
| `docs/internal/promotion-refusal-20261004/PLAN.md` | This task's durable source plan. |
| `docs/internal/promotion-refusal-20261004/HANDOFF.md` | This handoff. |

Not modified and why:

- Root `sql/executor/ddl.rs` — stale HeliosDB-Lite-era duplicate (last real change 2026-02), excluded from the published crate (`Cargo.toml` `exclude = [..., "/sql/"]`) and never compiled (`pub mod sql` resolves to `src/sql`).
- The SQL surface is not widened: HA status plans are still routed only on the `execute` path. The test observes the CHECK row through the already-public `sql::Executor`/`Parser` API instead of adding query-path routing.
- `RoleManager`, `WalApplicator::promote`, `failover_watcher` / legacy failover — not wired, not removed, no guard loosened, no writes redirected.

## 2. Scope of the guarantee — embedded SQL regression, NOT live acceptance

**This is an embedded SQL unit/regression change only.** It verifies that:

- `SWITCHOVER TO` is refused after the same primary + connected-standby
  preconditions the old placeholder checked, with an actionable message naming
  `unsupported`, fencing and `no role was changed`;
- `SWITCHOVER CHECK` runs through real SQL and its row is observed through the
  public plan-level Executor API: preserved seven-column schema (names and
  types), exactly one row, `can_proceed = Boolean(false)`, `target_healthy =
  Boolean(true)`, and `blockers` containing
  `"Runtime switchover/promotion is unsupported"`;
- neither statement mutates HA role, read-only state, rows or catalog.

**It does not claim and does not test any of:** a safe promotion, durable
old-primary fencing, live fencing, write admission, a durable source boundary,
a new history/epoch, online resync acceptance, catalog/auth/session serving
publication, or routing. No live two-node promotion was performed. The operator
contract in `docs/guides/replication-history-guard.md` is the authoritative
statement of what remains unsupported.

## 3. Precise unrun checks (in order)

Run inside `/home/gpc/HDB/worktrees/nano-promotion-refusal-20261004`, under the
`docs/GATES.md` heavy-op wrapper (flock build.lock + MemoryMax=24G), in the root
fleet schedule. None were run by this task. `docs/GATES.md` is authoritative for
the complete gate set.

```bash
# 1. changed files are fmt-clean
cargo fmt --all -- --check

# 2. cheapest compile+unit signal
cargo test --lib

# 3. the new regression target (own process; no ports, no filesystem paths)
cargo test --test promotion_refusal_tests

# 4. singleton-isolation sibling still passes
cargo test --test txn_replication_sync

# 5. full gate + doc tests + empty-suite check
cargo test --features internal-tests --tests --no-fail-fast -- --skip ha_tests::streaming_tests --skip lock_management
cargo test --doc
# empty-suite check per docs/GATES.md §3b:
awk '/^     Running/{s=$2} /^test result: ok\. 0 passed; 0 failed; 0 ignored/{print "EMPTY:", s}' <full.log>
```

### Clippy new-findings comparison — frozen baseline worktree, never `git stash`

Do **not** stash the active source. Use a separate frozen worktree at the merge
base with its own unique target dir, and a different unique target dir for this
tree, then compare normalized error summaries:

```bash
# frozen baseline tree (isolated; never the active worktree)
git worktree add --detach /tmp/nano-promotion-baseline 56b19dc42ce867a2314878543b3b156b6f92980b

# baseline clippy -> summary
CARGO_TARGET_DIR=/tmp/nano-promotion-baseline-target \
  cargo clippy --all-targets -- -D warnings 2>&1 | grep '^error' | sort | uniq -c > /tmp/base.txt
#   (run from /tmp/nano-promotion-baseline)

# this tree clippy -> summary (unique, different target dir)
CARGO_TARGET_DIR=/tmp/nano-promotion-mine-target \
  cargo clippy --all-targets -- -D warnings 2>&1 | grep '^error' | sort | uniq -c > /tmp/mine.txt

diff /tmp/base.txt /tmp/mine.txt
# rule: zero NEW findings is the pass. Never share a target dir between the two
# trees (stale artifacts); remove the baseline worktree afterwards.
```

Expected signals:

- `cargo test --test promotion_refusal_tests` → 1 passed, 0 failed.
- Empty-suite check: this target reports `ok. 1 passed` under default features
  (`ha-tier1` is a default feature). Under `--no-default-features` it is
  intentionally `cfg`-ed out, exactly like `tests/txn_replication_sync.rs`.
- clippy set-diff: no new findings.

## 4. Residual uncertainty (no build run)

1. The new test target has not been compiled. It uses public API already
   exercised elsewhere (`ha_state`, `NodeConfig`, `StandbyInfo`, `StandbyState`,
   `SyncMode`, `HARole`, `EmbeddedDatabase`, `Tuple`, `uuid`) plus the public
   `sql::{Executor, LogicalPlan, Parser}` surface (`Parser::parse_switchover_check_sql`,
   `Executor::new`, `Executor::execute_with_schema`); no new extern is required,
   but compilation is unproven.
2. The plan-level CHECK observation uses the public `EmbeddedDatabase.storage`
   field with `Executor::with_storage`, so it runs the handler against this
   database's own engine.
3. The refusal-message assertions (`unsupported`, `fencing`, `no role was
   changed`) are the actionable contract; a wording edit should keep those
   substrings or update the test.
4. `information_schema.tables` is assumed to expose the created table by
   `table_name`; used by existing tests (`tests/catalog_introspection_tests.rs`).

