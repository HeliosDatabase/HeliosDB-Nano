# IFNE - CREATE BRANCH IF NOT EXISTS

## Verdict

REAL BUG, fixed. `CREATE BRANCH IF NOT EXISTS <name> AS OF NOW` parsed `IF` as
the branch name, never created `<name>`, and the second identical statement
errored with `Branch 'IF' already exists`.

Baseline: Codex branch `fix/codex-ifne`, based on final main `52bdc71`.

## Baseline

Command:

```text
cargo test --test ifne_create_branch_if_not_exists -- --nocapture
```

Initial result before fix, before adding the later stress case: FAIL, 1 passed
/ 3 failed.

Observed failures:

- `CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW` created branch names
  `["IF", "main"]`; `feature_x` was missing.
- `CREATE DATABASE BRANCH IF NOT EXISTS feature_db FROM main AS OF NOW` created
  `IF`; the second call errored with `Branch 'IF' already exists`.
- Quoted branch names after `IF NOT EXISTS` hit the same idempotency failure.
- Plain duplicate `CREATE BRANCH plain_duplicate AS OF NOW` still errored, as it
  should.

Baseline performance: no useful before timing because the idempotency regression
fails immediately.

## Fix

- `src/sql/parser.rs`: parse and strip a real `IF NOT EXISTS` clause after
  `CREATE [DATABASE] BRANCH` before branch-name extraction.
- `src/sql/logical_plan.rs`: add `if_not_exists: bool` to
  `LogicalPlan::CreateBranch`.
- `src/sql/phase3/branching.rs` and `src/lib.rs`: thread the flag through the
  custom branch parser path.
- `src/sql/executor/phase3.rs`: when `if_not_exists` is set and the target
  branch already exists, return the normal empty DDL result instead of calling
  storage creation. Storage duplicate behavior remains unchanged for plain
  `CREATE BRANCH`.

## Tests

Regression file: `tests/ifne_create_branch_if_not_exists.rs`.

Coverage:

- Short form: `CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW`.
- Long form: `CREATE DATABASE BRANCH IF NOT EXISTS feature_db FROM main AS OF NOW`.
- Quoted branch name preserving existing quote-stripping behavior.
- Duplicate plain `CREATE BRANCH` still errors without `IF NOT EXISTS`.
- Stress/no-regression: 100 branch names, each created once and repeated once as
  a no-op, with no branch named `IF`.

After-fix results:

```text
cargo test --test ifne_create_branch_if_not_exists -- --nocapture
```

PASS, 5 passed / 0 failed. Stress: 100 first creates + 100 idempotent no-ops in
`62.598014ms`.

Additional checks:

- `cargo test --test v334_t4_branch_as_of_now_names -- --nocapture`: PASS, 5/5.
- `cargo test parse_create_branch --lib -- --nocapture`: PASS, 1/1.
- `cargo fmt --check`: PASS.
- `cargo test --lib --quiet`: FAIL, 1818 passed / 2 failed / 1 ignored.
  - Known unrelated failure:
    `vector::hnsw_index::tests::test_vector_count_tracking`.
  - Transient unrelated failure:
    `sql::query_cache::tests::test_cache_expiration`; rerun directly with
    `cargo test sql::query_cache::tests::test_cache_expiration --lib -- --nocapture`
    passed 1/1.

## Claude Comparison

Claude independently converged on the same production design: model
`if_not_exists` on `CreateBranch`, strip the clause in `parse_create_branch_sql`,
and no-op in the executor when the branch already exists. The main difference in
the Codex version is broader regression coverage, including quoted branch names,
plain-duplicate guard behavior, and a 200-statement stress path.

## Quality Score

94/100.

- Correctness: 35/35 - fixes the exact misparse and idempotency behavior without
  changing storage duplicate semantics.
- Regression coverage: 24/25 - covers short/long syntax, quoted names, duplicate
  guard, and stress; wire protocol is not separately needed because this is an
  embedded parser/executor bug.
- Performance: 14/15 - bounded stress path remains fast after fix; no completed
  before timing because the old behavior fails early.
- Scope control: 13/15 - touches the expected parser/logical/executor path plus
  one parser-call unit test update.
- Residual risk: 8/10 - branch parser remains a custom string parser, but this
  fix is narrow and explicitly tested across the supported branch syntaxes.
