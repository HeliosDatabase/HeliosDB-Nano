# Reproducing HeliosDB-Nano's benchmarks

**Short answer.** On a Linux x86-64 host with Rust, git, Python 3 and Docker, start
PostgreSQL 18.4 in a container, then run two commands from a checkout of the release:
`benches/public/release_bench.sh v4.41.1 out` (the 35-category pg35 suite and a primary-key
`UPDATE` over the PostgreSQL wire at 1 to 64 clients, about two hours) and
`docs/benchmarks/bench-engines.sh` with the two server binaries the first command built
(pgbench against PostgreSQL and both servers, about 20 minutes). Compare your
`out/RESULTS.md` with [`RELEASE_4_42_0.md`](RELEASE_4_42_0.md). Every published number came
from these commands; none was edited by hand.

This page says what each benchmark measures, what you need, the exact commands, how to check
that you measured the same binaries and reference we did, and how to read the results.

## What the benchmarks measure

| benchmark | question it answers | engine setup | client | script |
|---|---|---|---|---|
| **pg35** | How long does each of 35 kinds of SQL statement take, and how does that compare with PostgreSQL 18.4? | Nano embedded in the test process, in memory; PostgreSQL in a container over loopback TCP, `fsync=on`, `synchronous_commit=on` | the test itself | `benches/public/pg35_run.py` |
| **wire UPDATE** | How many one-row primary-key `UPDATE`s per second does a running server accept, prepared and as text, at 1 to 64 clients? | Nano server on an on-disk data directory, trust authentication, loopback | `examples/wire_pk_bench.rs` (`tokio-postgres`) | `benches/public/wire_pk_ab.sh` |
| **pgbench** | `SELECT 1` throughput, `COPY` load time, an indexed point read in three protocols, and `DROP TABLE` of 100k rows, for PostgreSQL and each Nano server | each engine as a server on its own port | `pgbench` and `psql` from the `postgres:18.4-bookworm` image | `docs/benchmarks/bench-engines.sh` |

`release_bench.sh` runs pg35 and the wire benchmark as A/B comparisons of two releases: it
clones the older revision out of the repository, builds both sides the same way and alternates
their order between rounds.

## What you need

| | used for the published run | needed by |
|---|---|---|
| Linux x86-64 | Rocky Linux 9.7, kernel 5.14, AMD EPYC 7371 (32 CPUs) | all |
| Rust toolchain | rustc 1.100.0, cargo 1.100.0 | builds |
| git, Python 3 (standard library only) | git 2.47, Python 3.9 | all |
| Docker | 29.3, `postgres:18.4-bookworm` image | the PostgreSQL reference; pgbench |
| Disk | about 5 GB for the two release build directories | builds |

The wire benchmark needs neither Docker nor `psql`. pg35 needs the PostgreSQL reference but no
`psql`.

## 1. Start the PostgreSQL reference

pg35 and pgbench compare against PostgreSQL 18.4 on `127.0.0.1:25433`, database `benchdb`,
user `bench`:

```
docker run -d --name pg_bench_nano -e POSTGRES_USER=bench -e POSTGRES_PASSWORD=benchpass \
    -e POSTGRES_DB=benchdb -p 25433:5432 postgres:18.4-bookworm
```

`pg35_run.py` connects before it builds anything and records the server's version, its
`system_identifier`, its start time and the settings the harness checks
(`pg35/postgres-preflight.json`). It refuses to start if the reference is missing.

## 2. Run the release benchmark (pg35 + wire UPDATE)

From the root of a checkout of the release:

```
benches/public/release_bench.sh v4.41.1 out
```

It runs five phases, each of which can be rerun alone with `PHASES="..."`:

1. **prepare** clones `v4.41.1` into `out/work/src-baseline` and copies the five pg35 harness
   files of this checkout over the clone (test files only; the older product source is not
   touched), so both sides run the same statements and the same result checks.
2. **build** builds both servers with `cargo build --release --locked` and both pg35 harnesses
   with the release profile, and records a build receipt and a source manifest for each.
3. **pg35** runs 2 rounds, alternating which release goes first (`PG35_ROUNDS`).
4. **wire** starts each server on a fresh data directory and runs the prepared and the text
   `UPDATE` at 1, 2, 4, 8, 16, 32 and 64 clients, 20 s each after 3 s of warm-up, 4 rounds in
   alternating order (`WIRE_ROUNDS`, `CLIENTS`, `SECONDS_PER_RUN`).
5. **report** writes `out/RESULTS.md`.

Each run waits until the host's 1-minute load average is below 6 (`MAX_LOAD`), for at most
30 minutes (`LOAD_WAIT`). On the 32-CPU host above the published run took about two hours:
about 50 minutes building both releases, 30 for pg35, 55 for the wire benchmark.

## 3. Run the pgbench comparison

With the two binaries step 2 built:

```
PROTOCOLS="simple extended prepared" DUR=8 CLIENTS="1 8 16 32 64" \
  docs/benchmarks/bench-engines.sh 4.41.1:out/bin/v4.41.1/heliosdb-nano 4.42.0:out/bin/v4.42.0/heliosdb-nano
```

Run it twice, the second time with the two Nano binaries in the opposite order; the published
tables are the mean of the two runs, with each run's ratio beside it.

## 4. Check that you measured what was published

- **Binaries.** `out/RESULTS.md` lists the sha256 of both servers and of the wire client. The
  published 4.42.0 server is the one the release gates built from the tagged commit; if your
  toolchain matches, your build should match it byte for byte. A different toolchain gives a
  different binary and slightly different numbers; the ratios between the two releases are what
  should agree.
- **pg35 receipts.** `out/pg35/arms/*/strict-receipt.json` is the harness's own record of each
  run: the binary and source hashes it ran, the PostgreSQL instance it measured, and every
  category's result check. A run whose result disagrees with the oracle produces no number.
- **Machine-readable copy.** `benches/public/export_results.py out [bench-engines-dir]` writes
  the whole run as one JSON document without file paths or host names; the published one is in
  `docs/benchmarks/results/`.

## 5. Read the results

- **pg35 times are per statement**; `x PG` is PostgreSQL's time divided by Nano's. The
  deployment differs (embedded and in memory against a server that syncs every commit), so the
  `x PG` column is not an engine-to-engine ratio for writes. The column to compare across
  releases is the time ratio between the two Nano builds.
- **Noise.** On a shared host two rounds of pg35 agree to within about 10% for reads; a ratio
  inside 0.9 to 1.1 is not a difference. Single-statement categories that commit (one-row
  `INSERT`, `UPDATE`, `DELETE`) are sensitive to background work; look at both rounds
  (`runs_us` in the JSON) before quoting a median.
- **Wire numbers** are medians of 4 rounds. `load` in the table is the host's load average
  when each round of the cell began; the 1-client cells begin right after a 64-client cell and
  carry its load.
- **What the benchmarks do not show**: durability-equal comparisons with PostgreSQL (see
  `heliosdb-nano-vs-postgresql-2026-08-28.md` for a wire-to-wire run), multi-statement
  transactions under contention, or datasets larger than memory.

## Frequently asked questions

**Is HeliosDB-Nano faster than PostgreSQL?** In pg35, embedded Nano is faster than PostgreSQL
18.4 in 34 of 35 categories and slower in one (the 4-table join). That compares an in-process,
in-memory engine with a server over TCP that syncs every commit; over the wire, with both as
servers, the margins are much smaller (pgbench tables in the release report).

**Why is 4.42.0 slower than 4.41.1 on single-row writes?** Since 4.42.0 a one-row `UPDATE` or
`DELETE` records the row's history and its snapshot in the same write as the row, so that
`AS OF` reads are correct after it; 4.41.1 skipped that history on its fast path. That is
several keys per commit where 4.41.1 wrote one. The release report lists the measured costs.

**Can I get the 4.41.1 write speed back?** For a store that does not use `AS OF`,
`VERSIONS BETWEEN` or branches from the past, set `[storage] time_travel_enabled = false`; the
single-row write path then writes what 4.41.1 wrote.

**How long does a full run take?** About two hours for `release_bench.sh` and 20 minutes for two
pgbench runs, on 32 CPUs.

**Can I compare other releases?** Yes: `release_bench.sh <any-earlier-tag>`. The pg35 harness
of the checkout is copied over the older tree, so the statements and checks are the same; a
release older than the strict harness may not compile with it.

**Where are the raw numbers?** In the output directory: `pg35/results.json`,
`wire/results.jsonl` (every run with its latency percentiles), and the pgbench `run-*.txt`
files; `export_results.py` gathers them into one JSON document.
