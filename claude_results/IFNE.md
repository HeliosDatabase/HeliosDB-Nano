# IFNE Result — CREATE BRANCH IF NOT EXISTS (follow-up bug)

## Verdict

**REAL BUG — fixed.** (Follow-up discovered during T4.) Both agents produced an **identical
production fix** independently; Codex's superset (cleaner helper + broader tests)
is integrated.

Baseline: branch from `final/main` `52bdc71` (all 13 items + docs).

## Root cause

`parse_create_branch_sql` read the first token after `CREATE [DATABASE] BRANCH`
as the branch name, so `CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW` created
a branch literally named **`IF`**, never created `feature_x`, and a second call
errored ("Branch 'IF' already exists") — no idempotency. `IF NOT EXISTS` was not
modeled in `LogicalPlan::CreateBranch`.

Reproduced (both agents): `branch_names()` → `["IF", "main"]`.

## Fix (converged; Codex `b42272b` integrated)

Both agents independently wrote the same shape:
- `parser.rs`: detect + strip a real `IF NOT EXISTS` after `CREATE [DATABASE]
  BRANCH` (before name parsing) and return `if_not_exists`.
- `logical_plan.rs`: add `if_not_exists: bool` to `LogicalPlan::CreateBranch`.
- `lib.rs` + `BranchingParser::parse_create_branch`: thread the flag.
- `phase3.rs` executor: when `if_not_exists` and `storage.get_branch(name).is_ok()`,
  return the normal empty DDL result (no-op) instead of erroring; storage's
  duplicate behavior is otherwise unchanged.

Codex's version additionally factored the empty-DDL result into an
`empty_phase3_result` helper (avoiding the duplicated `ScanOperator` my inline
version had) and added quoted-name + duplicate-guard + stress coverage — so its
patch is the superset and is the integrated one.

## Tests (`tests/ifne_create_branch_if_not_exists.rs`, Codex superset)

Short form → real name; `CREATE DATABASE BRANCH IF NOT EXISTS … FROM main AS OF
NOW`; quoted name after `IF NOT EXISTS`; idempotent second call (no-op); plain
duplicate without the clause still errors; 100 creates + 100 idempotent no-ops
stress. (Claude's `v334_ifne_create_branch.rs` 3-test set is a subset.)

## Verification (final main)

- `cargo test --test ifne_create_branch_if_not_exists`: PASS.
- `cargo test --test v334_t4_branch_as_of_now_names`: 5/5 (the now-fixed
  `IF NOT EXISTS` complements the T4 already-fixed plain forms).
- `cargo test --lib`: 1819 pass / 1 pre-existing hnsw / 1 ignored.
- fmt PASS; clippy only the pre-existing `streaming.rs:69`.

## Cross-agent note

Cleanest convergence of the whole run: both independent production fixes were
identical. Codex's helper + broader tests made its patch the superset. This
follow-up was itself a product of the loop (Codex found it during T4; Claude
filed and characterized it). Quality (Codex) 94.
