**A crash-recovery torture harness: a seeded workload is killed with SIGKILL at chosen and at random points, and the reopened database is checked against a model of what was acknowledged.**

The existing crash tests let the child process finish its statements and exit; none killed a process in the middle of its work.

A worker process runs a seeded workload (transfers with a constant sum, multi-row inserts and updates, deletes, rolled-back transactions) and writes down each operation that was acknowledged. It is killed, by itself at a named point of an operation (replayable from the seed) or from outside at a seeded moment. A fresh process opens the database and the harness checks: every acknowledged operation is present; the operation in flight is wholly present or wholly absent; the sum of balances is unchanged; index lookups agree with full reads; the foreign key is still enforced; generated ids are not reused; the branch taken at the start, and a time-travel read, still answer with the opening state. Three deliberate faults in the worker prove that the checker fails when it should. No engine code was changed: the kill is a signal to a whole process.

A fixed list of seeds runs in the ordinary test suite. `scripts/crash_torture_run.sh <iterations> [first-seed] [time-budget-seconds]` runs the long version and writes `docs/correctness/crash-recovery.md`. The harness covers a process crash, not power loss.

Tests: `tests/crash_torture.rs`.
