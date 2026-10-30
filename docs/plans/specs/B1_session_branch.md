# B1 — the active branch is session state

Plan: `docs/plans/BRANCH_ISOLATION.md` (D1, part of D6). Sprinter 6bc4a4040eb7 items 3, 7 and
the dropped-branch lookup. Later phases (B2 writes/constraints, B3 schema, B4 fork point)
build on the one accessor this phase introduces, so get its shape right.

## Facts (survey 2026-10-30; line numbers approximate)

- `StorageEngine.current_branch: Arc<Mutex<Option<String>>>` (engine.rs ~2029) is the ONLY
  branch selector, engine-wide. `set_current_branch` (~16545) also bumps the schema
  generation and clears the whole row cache; `get_current_branch_id` (~16576) resolves the
  name through `get_branch_by_name` on every call; `is_branch_active` is read on many fast
  paths (scan.rs, executor/mod.rs, join.rs, explain.rs, lib.rs fast paths).
- Writers: `USE BRANCH` (lib.rs ~8223 → phase3.rs `handle_use_branch` ~994 → engine
  `use_branch` ~11280); REPL `\use` and its USE pre-detect (repl/shell.rs ~195, ~315: no
  existence check); `switch_branch` (lib.rs ~26087); MCP `with_branch` switch-and-restore
  (mcp/tools.rs ~416); `CodeGraphBranchGuard` (lib.rs ~2217).
- Implicit readers of "the current branch": `CREATE BRANCH` without `PARENT`
  (phase3.rs ~175), `merge_branch` "source into current" (lib.rs ~26104), SHOW/`\show
  branch` and system functions/views that report it.
- Per-connection state already exists: `crate::session::scoped::SessionScopedState`
  (src/session/scoped.rs), installed per statement in the thread-local
  `SESSION_SCOPED_STATE` (lib.rs ~593) by `session_statement_context_guard` (~23359);
  session-less funnels (embedded, REPL, REST, MCP) install the handle-wide
  `embedded_scoped` with `install_if_absent`. Precedents for moving engine-wide state onto
  it: the tenant binding (`bind_tenant`, `tenant_bound` atomic mirror, sprinter
  d03de7fc3b22) and `bulk_load_mode` (`is_bulk_load_mode` reads TLS first, engine.rs
  ~4081).
- Caches that assume "a switch is global and clears everything": the row cache (keyed
  `(table,row_id)`), the result cache (SQL text + login + epoch, lib.rs ~4127-4145), the
  plan cache (invalidated globally on `UseBranch`, lib.rs ~2432), the schema generation /
  existence cache, the W2.5 watermark fast path `txn_base_tuples` (scan.rs ~177).
- REST: `/v1/branches/:name/query` checks the branch exists (api/handlers/query_handler.rs
  ~67, ~217) and then runs on main; data_handler.rs ~41 says data operations use main.
- PostgreSQL startup: `database` selects a TENANT (handler.rs ~1560-1600).
- Dropped branches: `get_branch_by_name` has no state filter, so a dropped branch can be
  `USE`d and written, `CREATE BRANCH x` after a drop fails "already exists", and
  `CREATE BRANCH IF NOT EXISTS x` no-ops onto the dropped branch's rows (phase3.rs ~129).

## What to build

1. **Binding on the session.** Add the active branch to `SessionScopedState`: the branch's
   name and id plus its ancestor chain (ids, oldest first; B4 adds each level's anchor),
   behind a lock, with a lock-free mirror (`on_branch: AtomicBool`, and the id in an
   `AtomicU64` if useful) for the per-statement and per-row fast-path checks. `None` means
   main.
2. **One accessor, no global.** Remove `StorageEngine.current_branch` and
   `set_current_branch`. Every reader goes through one engine accessor (for example
   `active_branch() -> Option<BranchBinding>` and `is_branch_active() -> bool`) that reads
   the installed session state. With no session installed (engine-internal work: MV
   scheduler, version GC, the physical applier, recovery) the answer is main. Grep for
   every former use of `current_branch`, `get_current_branch*`, `is_branch_active`,
   `set_current_branch`, `use_branch`, `switch_branch` and convert each; none may keep a
   private copy.
3. **Who sets it.** `USE BRANCH x` / `USE DATABASE BRANCH x`, REPL `\use x`, embedded
   `switch_branch(x)`: resolve `x` to an ACTIVE branch (see 5) and bind it on the current
   session; `main` unbinds. Unknown or not-active: SQLSTATE `3D000`, message naming the
   branch and `SHOW BRANCHES`. Also accept `SET heliosdb.branch = 'x'` (and `TO`),
   `RESET heliosdb.branch`, `SHOW heliosdb.branch`, `current_setting('heliosdb.branch')`
   as the same binding (they must agree with `USE BRANCH`). MySQL wire: the same
   statements, per connection.
   **Selecting a branch at PostgreSQL startup** (HeliosProxy relays the client's startup
   verbatim and keys its pools and result cache on the `database` value, so a branch
   named there is isolated through the proxy with no proxy change):
   - `database=<db>@<branch>`: first resolve the WHOLE value exactly as today (reserved
     name or tenant name) — if it resolves, it means what it means today and selects no
     branch. Otherwise split at the FIRST `@`: `<db>` must resolve as today (reserved
     name such as `postgres`, or a tenant, bound as today) and `<branch>` must be an
     active branch (5), bound on the new session. Either part failing: FATAL `3D000`
     `database "<value>" does not exist` (log the reason, as the tenant path does).
   - `options=-c heliosdb.branch=<branch>` (PostgreSQL's standard way to set a parameter
     at startup): validated like `USE`.
   - Both given and naming different branches: FATAL `3D000` naming both. All refusals
     happen before ReadyForQuery. `ParameterStatus` need not report the branch.
4. **Implicit readers use the session.** `CREATE BRANCH` without `PARENT`: parent = the
   session's branch. Library/SQL merge "into current": the session's branch. SHOW BRANCH /
   `\show branch` / any function or view reporting the current branch: the session's.
5. **Dropped and merged branches.** One resolver for name → active branch used by every
   binding and by CREATE/IF NOT EXISTS: a `Dropped` or `Merged` branch is not found for
   binding; its name is free again — `CREATE BRANCH x` creates a NEW branch with a NEW id
   (verify ids are never reused, including after a restart; the old overlay keys use the
   old id so nothing resurrects), and `IF NOT EXISTS` over a dropped name creates one too.
   System views keep listing merged branches as today. A session bound to a branch that is
   dropped or merged by anyone fails CLOSED: its next statement is refused `3D000`
   ("branch x was dropped; USE BRANCH main or another branch") and nothing it runs reaches
   main or any other branch; the binding stays until the session changes it. Make the
   check cheap (a global branch-registry generation counter bumped by drop/merge; the
   binding re-validates only when it changed). `DROP BRANCH x` while THIS session is bound
   to `x` is refused `55006` (as PostgreSQL refuses dropping the open database); dropping
   a branch other sessions are bound to is allowed (they fail closed as above).
6. **Caches.** Nothing cached for one branch is served to another or to main:
   - row cache: bypass (no read, no fill, no invalidation dependence) whenever the session
     is on a branch;
   - result cache: bypass on a branch (like the tenant gate), so main's entries are never
     served to a branch session and a branch's are never stored;
   - plan cache and prepared statements: a plan's correctness must not depend on the
     branch at planning time. Either plans contain no branch decision (every fast path
     checks the binding at execution), or the cache key includes the branch id. A
     statement prepared on main and executed after `USE BRANCH b` reads `b`, and the
     reverse. Remove the global plan-cache flush on `USE BRANCH` once this holds.
   - W2.5 `txn_base_tuples` and any other "cleared on switch" assumption: skip the fast
     path off main explicitly.
   - schema generation bump on switch: no longer needed for correctness in B1 (schema is
     still global until B3); keep nothing that depends on it.
7. **REST.** Every route under `/v1/branches/:name/…` that runs SQL or touches data runs
   it with a request-scoped session state bound to `:name` (resolved as in 5; 404 when not
   active), and every other route on main. No global switch, so concurrent requests on
   different branches never interfere.
8. **MCP.** Tools that take a branch run with a call-scoped binding instead of
   `with_branch`'s switch-and-restore. Add `heliosdb_branch_drop` (name; refuses `main`),
   and update the tool catalog count wherever docs state it.
9. **Code graph.** `CodeGraphBranchGuard` becomes a scoped binding for its duration on the
   calling session (or a private session state), never a global switch.

## Tests (write them first; they must fail on the current tree)

New file `tests/branch_session_isolation.rs` (use the two-connection harness pattern of
`tests/session_scoping_batch_g2.rs` ~406-430 — `PgServer::new` + two `tokio_postgres`
clients — and `create_session` + `execute_for_session` for embedded):
- Two PG connections, simple protocol: A `USE BRANCH b` then A reads the branch's rows; B
  still reads main's and its INSERT lands on main (verified after `DROP BRANCH b` from a
  third connection); B `USE BRANCH main` does not move A.
- Same with the extended protocol, including a statement PREPARED before `USE BRANCH b`
  and executed after it (reads `b`), and one prepared on `b` executed after `USE BRANCH
  main` (reads main).
- Result cache: the same `SELECT` text on main and on a branch with different data returns
  each its own rows, in both orders.
- Row cache: a point SELECT by primary key after an UPDATE on the branch returns the
  branch's value on the branch session and main's value on a main session.
- Startup: `dbname=postgres@b` reads `b` and `dbname=postgres` reads main on the same
  server; a tenant `t` with `dbname=t@b` binds the tenant AND the branch; a tenant whose
  own name contains `@` still resolves as itself; `options=-c heliosdb.branch=b` reads
  `b`; an unknown branch in either form is refused 3D000; the two forms disagreeing is
  refused 3D000;
  `SET heliosdb.branch`, `SHOW heliosdb.branch`, `RESET` agree with `USE BRANCH`.
- Two embedded sessions (`create_session` + `execute_for_session`) on different branches.
- Dropped branches: `USE` of a dropped branch 3D000; a session bound to `b` while another
  session drops `b` gets 3D000 on its next statement (SELECT and INSERT), and main's row
  count is unchanged; `DROP BRANCH b` on the session bound to `b` 55006; `CREATE BRANCH
  b` after the drop starts with none of the old branch's rows; `CREATE BRANCH IF NOT
  EXISTS b` over a dropped `b` creates a fresh one.
- REST: 40 concurrent requests alternating `/v1/branches/b/query` and the main query
  route each read their own branch's rows; a write through the branch route lands on the
  branch only.
- MCP: a branch-scoped tool call reads the branch while a concurrent main call reads main;
  `heliosdb_branch_drop` drops a branch and refuses `main`.
- `CREATE BRANCH c` without PARENT from a session on `b` has parent `b`; the same from a
  session on main has parent main.
- REPL `\use nosuch` is refused (unit-level if the REPL is hard to drive).
All existing branch tests must keep passing: tests/branch_sql_integration_tests.rs,
branch_data_isolation_test.rs, branch_merge_surface_tests.rs, branch_as_of_tests.rs,
code_graph_on_branch.rs, ifne_create_branch_if_not_exists.rs,
v334_t4_branch_as_of_now_names.rs, td4_show_branches.rs,
w3_5_alter_snapshot_characterization.rs, session_scoping_batch_g2.rs.

## Out of scope here

Write-path isolation and constraints (B2), schema DDL (B3), fork-point reads and reclaim
(B4). Do not change merge semantics beyond "current = the session's branch".

## Rules

Do not run cargo, git or any build; the session that dispatched you runs every build and
gate. Keep both DML executor families consistent. No new `unwrap`/`expect` on non-test
paths. Report: files touched, the accessor's final shape, every former global reader and
what it became, any reader you could not convert and why, and anything in this spec that
turned out wrong.
