**A SQL Logic Test runner: the public corpus runs against the embedded API, the PostgreSQL wire and the MySQL wire, with a per-directory score, the largest failure classes and a baseline that may not go down.**

There was no measurement of how many standard SQL queries HeliosDB-Nano answers correctly.

`scripts/slt_fetch_corpus.sh` downloads the public SQL Logic Test corpus at a pinned revision and checks it against a pinned digest; the corpus is not stored in the repository. `scripts/slt_run.sh [embedded] [pgwire] [mysqlwire]` runs every script of it, each in its own process, with a time limit per record. A record that hangs or kills its process is recorded and the rest of the file still runs. The run honours the corpus's `skipif` / `onlyif` engine labels (the embedded API and the PostgreSQL wire run as `postgresql`, the MySQL wire as `mysql`). It writes `docs/correctness/sqllogictest.<interface>.json` and `docs/correctness/sqllogictest.md` (passed, failed, skipped, timed out and crashed per corpus directory; the ten largest failure classes with one example each), and with `SLT_BASELINE=check` it fails when a directory passes fewer records than `docs/correctness/sqllogictest_baseline.json` holds.

A small corpus in the same format (453 records: joins, aggregates, NULL ordering, set operations, subqueries) is in `tests/fixtures/slt/` and runs through all three interfaces in the ordinary test suite, against its own baseline.

Tests: `tests/slt_runner.rs`.
