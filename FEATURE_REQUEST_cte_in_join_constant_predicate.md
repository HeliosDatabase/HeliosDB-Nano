---
requested-by: v3.22.3 release-workflow CI surface — danimoya
requested-against: HeliosDB-Nano v3.22.3 (latent across earlier versions)
priority: medium
status: open
date-filed: 2026-05-01
track: sql / planner
---

# Bug: CTE in JOIN with constant-predicate ON clause degenerates to cross product

## TL;DR

```sql
WITH eng AS (SELECT id, name, salary FROM employees WHERE dept = 'Engineering')
SELECT eng.name, departments.budget
  FROM eng JOIN departments ON departments.name = 'Engineering';
```

Expected: `eng` (3 rows) joined with `departments` filtered to `name = 'Engineering'`
(1 row) ⇒ 3 result rows.

Actual: planner emits a full cross product — 3 × 3 = 9 result rows. The
constant predicate on the right side is not being pushed down as a filter
before the join.

## Discovery

Surfaced when the v3.22.3 release workflow ran the integration test
suite for the first time on a tag push. Test
`tests/cte_hardening_tests.rs::cte_hardening::test_basic_cte_used_in_join`
fails with `assertion left == right failed: left: 9, right: 3`.

Latent bug — the test was added in commit `eda2290 fix: 3 transaction
bugs + 8 clippy fixes + 182 hardening tests` along with 181 other
hardening tests. It has been failing since that landing. Lib tests
(1746) and the rest of the integration suite were passing in earlier
manual runs because no CI ran integration tests on push prior to v3.22.3.

## Hypothesis

The planner's join-condition handling treats `ON <const_predicate>`
differently from `WHERE <const_predicate>`. With no actual join key in
the ON clause, two reasonable behaviours exist:

1. Treat ON as a WHERE on the right side, then cross-join — yields 3 rows.
2. Cross-join without filtering — yields 9 rows.

Today we get (2). PostgreSQL semantics are (1) (the predicate restricts
the inner side before the cross-product). Likely fix: in the planner's
join-clause analysis, recognise predicates that reference only one side
and push them down to a Filter node above that side's source before
forming the join.

Suggestive search target: `src/sql/planner.rs` — the JOIN normalisation
path; look for where ON predicates are partitioned into `equi-join keys`
vs. `residual conditions`.

## Acceptance criteria

- [ ] `tests/cte_hardening_tests.rs::cte_hardening::test_basic_cte_used_in_join`
      passes after fix (currently `#[ignore]`).
- [ ] Remove the `#[ignore]` attribute and the corresponding
      `FEATURE_REQUEST_cte_in_join_constant_predicate.md` reference.
- [ ] No regression on the other 37 cte_hardening tests.
- [ ] Add a parametric variant covering JOIN with a one-sided predicate
      that is not constant (e.g. `ON departments.budget > 100000`) — the
      same push-down rule should apply.

## Workaround

Restructure the query so the predicate is in WHERE, not ON, or use
explicit subquery filtering on the inner side:

```sql
-- portable equivalent that produces 3 rows on Nano today:
WITH eng AS (SELECT id, name, salary FROM employees WHERE dept = 'Engineering'),
     eng_dept AS (SELECT * FROM departments WHERE name = 'Engineering')
SELECT eng.name, eng_dept.budget FROM eng CROSS JOIN eng_dept;
```

## Impact / blast radius

Affects any JOIN whose ON clause contains only one-sided predicates
(constants, single-table column comparisons). Most real-world JOINs use
join-key predicates, so production exposure is likely small — but the
behavioural divergence from PostgreSQL is a correctness gap.
