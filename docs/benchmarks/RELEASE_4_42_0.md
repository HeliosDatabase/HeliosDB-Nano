# HeliosDB-Nano 4.42.0 against 4.41.1: the release benchmark

Three measurements of the 4.42.0 release build against the 4.41.1 release build, taken on
2026-11-17 on one host, with the commands at the end of this page. Every table is copied from the
output of those commands; nothing in them was edited by hand.

| part | what it measures | how |
|---|---|---|
| pg35 | 35 SQL categories, Nano embedded and in memory, beside PostgreSQL 18.4 | `benches/public/pg35_run.py` |
| wire | a one-row `UPDATE` by primary key against a running server, prepared and as text, 1 to 64 clients | `benches/public/wire_pk_ab.sh` |
| pgbench | `SELECT 1`, `COPY`, an indexed point read in three protocols, `DROP TABLE`, against PostgreSQL and both servers | `docs/benchmarks/bench-engines.sh` |

## What the numbers say

- **Against PostgreSQL 18.4** (pg35): both releases are faster in 34 of the 35 categories and slower
  in one, the 4-table join (0.8x for 4.41.1, 0.5x for 4.42.0). Read the deployment note under the
  table before quoting a ratio: Nano runs in the test process and in memory, PostgreSQL is a server
  over TCP that syncs every commit.
- **Reads against 4.41.1**: a point lookup, a scan with a filter, the two-table joins, subqueries,
  CTEs, window functions and `ORDER BY … LIMIT` are within about 10% either way, which is also
  what two rounds on this host can tell apart. The 4-table join is 1.4x slower, `UNION` 1.2x,
  string functions 1.2x.
- **Single-row writes are slower than in 4.41.1**: over the wire a primary-key `UPDATE` runs at 0.71
  of 4.41.1 with one client and 0.46 with 64; embedded, `UPDATE`, `DELETE` and `INSERT` of one row
  take 2.3 to 4.0 times as long. Most of it is the row history each commit now records; see
  *What is slower, and why*.
- **Schema statements are slower**: `DROP TABLE` 2.4x, `CREATE TABLE` 2.2x, the others 1.4x to 1.7x.
  `ALTER TABLE` is faster (0.72x).
- **`INSERT … SELECT` is seven times faster** (196 µs against 1.41 ms).
- **pgbench**: `SELECT 1` and `COPY` are within the spread between two runs; the indexed point read
  runs at 0.91–0.98 of 4.41.1 as text, 0.90–0.92 in the extended protocol and 0.85–0.88 prepared.

Three regressions that the first run of these benchmarks found were fixed before this run
(CHANGELOG, *Performance — three regressions against 4.41.1*); the numbers here are after those
fixes.

## What was measured

| | |
|---|---|
| date (UTC) | 2026-11-17T15:39:36Z |
| host | Linux 5.14.0-611.34.1.el9_7.x86_64 x86_64, AMD EPYC 7371 16-Core Processor, 32 CPUs |
| toolchain | rustc 1.100.0 (0ef1bc386 2026-11-09); cargo 1.100.0 (3acc79c0c 2026-11-05) |
| v4.42.0 | commit `2a5d741aab9e525af506064e98084508de1510ce`, 2 uncommitted file(s) |
| v4.41.1 | commit `4af19e1c447b8b9f8a621a984e8cc0558195bf3b`, pg35 harness files copied from v4.42.0 |
| `bin/v4.42.0/heliosdb-nano` | sha256 `62cd542ea33ffbe95c88d27b5da5e469fdb519523d8aa356ed8ced2ba036479e` |
| `bin/v4.41.1/heliosdb-nano` | sha256 `956bc158fd90f677724a2f4133e1594a1cdaa75154ff44d38817ef5beddef947` |
| `bin/wire_pk_bench` | sha256 `e303016e9f70ede4e9acf35b842be259b73bf610cd5a14cd87dbc92c2eba970f` |

Both server binaries and both pg35 harnesses are built with `--release --locked` (LTO, one codegen unit).

The two uncommitted files are `.gitignore` (a local edit) and one untracked local note; neither is
compiled. The 4.42.0 server binary is byte-identical to the one the release gates built and tested.

The host runs other workloads. Each run waits for the 1-minute load to fall below 6 before it
starts, and the two releases alternate, so a busy minute lands on both; the ratios between the
releases are steadier than the absolute numbers.

## pg35: 35 SQL categories beside PostgreSQL 18.4

pg35: 35 categories, 300 timed iterations each (after 2 warm-ups), median of 2 round(s).

Nano runs embedded and in memory inside the test process. PostgreSQL is `PostgreSQL 18.4 (Debian 18.4-1.pgdg12+1)`, a server reached over loopback TCP with `fsync=on`, `synchronous_commit=on`. Times are per statement; `x PG` is PostgreSQL's time divided by Nano's.

| category | PostgreSQL | v4.41.1 | x PG | v4.42.0 | x PG | v4.42.0 / v4.41.1 (time) |
|---|---:|---:|---:|---:|---:|---:|
| CREATE TABLE | 30.97 ms | 158.0 us | 196.0 | 344.3 us | 89.9 | 2.18 |
| CREATE INDEX | 29.34 ms | 292.5 us | 100.3 | 480.1 us | 61.1 | 1.64 |
| ALTER TABLE | 30.38 ms | 899.8 us | 33.8 | 649.7 us | 46.8 | 0.72 |
| DROP TABLE | 14.43 ms | 149.6 us | 96.5 | 359.2 us | 40.2 | 2.40 |
| CREATE/DROP VIEW | 29.57 ms | 128.5 us | 230.1 | 182.2 us | 162.3 | 1.42 |
| REFRESH MATVIEW | 18.61 ms | 538.6 us | 34.6 | 839.0 us | 22.2 | 1.56 |
| TRUNCATE | 91.20 ms | 156.7 us | 581.8 | 261.2 us | 349.2 | 1.67 |
| INSERT single | 14.87 ms | 7.10 us | 2,095.6 | 28.44 us | 523.0 | 4.01 |
| INSERT multi-row | 14.40 ms | 150.4 us | 95.7 | 218.6 us | 65.9 | 1.45 |
| INSERT..SELECT | 15.06 ms | 1.41 ms | 10.7 | 196.2 us | 76.7 | 0.14 |
| UPDATE point | 15.01 ms | 9.90 us | 1,516.4 | 37.77 us | 397.4 | 3.82 |
| DELETE point | 15.41 ms | 5.18 us | 2,976.3 | 11.92 us | 1,293.2 | 2.30 |
| UPSERT | 14.72 ms | 61.53 us | 239.2 | 94.43 us | 155.9 | 1.53 |
| UPDATE+subquery | 15.08 ms | 138.2 us | 109.1 | 190.9 us | 79.0 | 1.38 |
| Point lookup | 195.4 us | 3.20 us | 61.0 | 2.99 us | 65.3 | 0.93 |
| Full scan+filter | 254.2 us | 27.65 us | 9.2 | 28.11 us | 9.0 | 1.02 |
| Aggregation | 274.3 us | 1.15 us | 238.1 | 1.28 us | 215.1 | 1.11 |
| INNER JOIN | 249.7 us | 24.47 us | 10.2 | 25.72 us | 9.7 | 1.05 |
| LEFT JOIN | 249.9 us | 19.24 us | 13.0 | 20.67 us | 12.1 | 1.07 |
| 4-table JOIN | 476.3 us | 632.4 us | 0.8 | 885.6 us | 0.5 | 1.40 |
| Scalar subquery | 334.9 us | 2.83 us | 118.5 | 3.02 us | 110.9 | 1.07 |
| EXISTS subquery | 444.4 us | 5.40 us | 82.3 | 4.76 us | 93.3 | 0.88 |
| IN subquery | 376.6 us | 5.44 us | 69.2 | 5.25 us | 71.7 | 0.97 |
| CTE | 443.2 us | 6.46 us | 68.6 | 5.72 us | 77.5 | 0.89 |
| Recursive CTE | 295.5 us | 3.16 us | 93.4 | 2.97 us | 99.6 | 0.94 |
| Window funcs | 502.9 us | 19.71 us | 25.5 | 20.72 us | 24.3 | 1.05 |
| UNION | 312.6 us | 7.52 us | 41.6 | 9.27 us | 33.7 | 1.23 |
| DISTINCT | 210.6 us | 0.73 us | 289.3 | 0.86 us | 243.6 | 1.19 |
| ORDER+LIMIT | 275.2 us | 91.83 us | 3.0 | 94.72 us | 2.9 | 1.03 |
| CASE expr | 237.8 us | 16.76 us | 14.2 | 16.50 us | 14.4 | 0.98 |
| LIKE/BETWEEN/IN | 257.3 us | 11.78 us | 21.8 | 11.52 us | 22.3 | 0.98 |
| String ops | 237.9 us | 12.07 us | 19.7 | 14.14 us | 16.8 | 1.17 |
| Transaction ctl | 15.50 ms | 49.24 us | 314.7 | 61.83 us | 250.7 | 1.26 |
| Prepared stmts | 451.5 us | 3.46 us | 130.7 | 5.01 us | 90.2 | 1.45 |
| SET/SHOW/RESET | 346.7 us | 1.93 us | 179.9 | 3.03 us | 114.3 | 1.57 |

- `v4.41.1`: faster than PostgreSQL in 34 of 35 categories.
- `v4.42.0`: faster than PostgreSQL in 34 of 35 categories.

| run | load before | load after |
|---|---:|---:|
| v4_41_1-r1 | 1.9 | 2.27 |
| v4_42_0-r1 | 2.27 | 2.6 |
| v4_42_0-r2 | 2.6 | 2.58 |
| v4_41_1-r2 | 2.58 | 2.23 |

The harness checks every statement's result against an independent model on both engines; a run
with a wrong result does not produce a number. A cell is the mean of 300 timed statements.

## Wire: one-row UPDATE by primary key against a running server

`extended` is a prepared statement bound per execution; `simple` is the statement sent as text.
Client and server share the host. Each client has its own connection and its own rows.

Median of 4 round(s); ratio is ops/s against `v4.41.1`.

| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |
|---|---|---:|---|---:|---:|---:|---:|---:|---|
| update | extended | 1 | v4.41.1 | 13,632 | 1.00 | 71 | 91 | 0 | 2.23/15.34/14.26/15.07 |
| update | extended | 1 | v4.42.0 | 9,740 | 0.71 | 100 | 130 | 0 | 15.99/14.41/14.65/15.19 |
| update | extended | 2 | v4.41.1 | 20,951 | 1.00 | 90 | 115 | 0 | 1.95/11.58/10.91/10.87 |
| update | extended | 2 | v4.42.0 | 12,313 | 0.59 | 158 | 223 | 0 | 11.59/10.96/11.16/11.43 |
| update | extended | 4 | v4.41.1 | 37,839 | 1.00 | 96 | 175 | 0 | 2.51/8.38/7.79/8.59 |
| update | extended | 4 | v4.42.0 | 24,603 | 0.65 | 151 | 273 | 0 | 8.38/8.3/8.12/8.39 |
| update | extended | 8 | v4.41.1 | 62,453 | 1.00 | 120 | 204 | 0 | 3.4/7.24/7.28/7.25 |
| update | extended | 8 | v4.42.0 | 41,894 | 0.67 | 180 | 375 | 0 | 7.48/7.73/7.46/7.15 |
| update | extended | 16 | v4.41.1 | 107,408 | 1.00 | 138 | 310 | 0 | 5.16/8.1/8.07/7.68 |
| update | extended | 16 | v4.42.0 | 67,019 | 0.62 | 223 | 610 | 0 | 8.19/8.29/8.27/8.01 |
| update | extended | 32 | v4.41.1 | 142,393 | 1.00 | 199 | 642 | 0 | 7.43/9.55/10.08/10.08 |
| update | extended | 32 | v4.42.0 | 70,281 | 0.49 | 394 | 1,054 | 0 | 10.72/10.92/11.42/11.24 |
| update | extended | 64 | v4.41.1 | 146,756 | 1.00 | 397 | 1,084 | 0 | 10.89/11.86/13.31/13.39 |
| update | extended | 64 | v4.42.0 | 68,153 | 0.46 | 824 | 5,764 | 0 | 14.98/13.94/13.2/14.4 |
| update | simple | 1 | v4.41.1 | 12,560 | 1.00 | 76 | 96 | 0 | 13.78/16.21/15.9/16.2 |
| update | simple | 1 | v4.42.0 | 7,766 | 0.62 | 122 | 194 | 0 | 17.83/16.2/16.23/16.9 |
| update | simple | 2 | v4.41.1 | 19,248 | 1.00 | 98 | 124 | 0 | 10.54/11.33/11.95/11.18 |
| update | simple | 2 | v4.42.0 | 10,252 | 0.53 | 190 | 257 | 0 | 13.32/12.42/12.3/11.93 |
| update | simple | 4 | v4.41.1 | 37,895 | 1.00 | 96 | 176 | 0 | 7.77/8.81/8.97/8.94 |
| update | simple | 4 | v4.42.0 | 22,261 | 0.59 | 167 | 308 | 0 | 9.81/9.15/9.2/8.98 |
| update | simple | 8 | v4.41.1 | 63,676 | 1.00 | 118 | 198 | 0 | 6.71/7.28/7.33/7.7 |
| update | simple | 8 | v4.42.0 | 40,822 | 0.64 | 186 | 408 | 0 | 8.54/7.63/8.1/8.06 |
| update | simple | 16 | v4.41.1 | 110,803 | 1.00 | 134 | 310 | 0 | 7.23/7.1/7.42/7.61 |
| update | simple | 16 | v4.42.0 | 65,884 | 0.59 | 226 | 630 | 0 | 8.71/8.14/8.54/8.73 |
| update | simple | 32 | v4.41.1 | 145,726 | 1.00 | 195 | 636 | 0 | 9.87/9.99/9.32/8.61 |
| update | simple | 32 | v4.42.0 | 73,248 | 0.50 | 386 | 996 | 0 | 11.01/11.55/11.69/11.43 |
| update | simple | 64 | v4.41.1 | 150,037 | 1.00 | 390 | 1,060 | 0 | 13.6/14.46/13.21/11.83 |
| update | simple | 64 | v4.42.0 | 68,510 | 0.46 | 838 | 4,477 | 0 | 14.35/14.85/14.29/13.59 |

How to read it:

- `load` is the host's 1-minute load when each of the four rounds of that cell began. It is mostly
  this benchmark's own earlier cells: a 1-client cell begins right after a 64-client cell, and the
  load average takes minutes to fall.
- Each server runs the prepared cells from 1 to 64 clients and then the text cells, so the text
  cells at 1, 2 and 4 clients start right after 64 clients of prepared writes on the same server.
  That is why text is below prepared there on 4.42.0. Run alone at 1, 2 and 4 clients the same
  binary measures 8,860 / 11,292 / 24,989 ops/s as text against 9,885 / 12,786 / 25,324 prepared
  (2 rounds of 10 s).
- p99 at 64 clients is 5.8 ms prepared and 4.5 ms as text, against 1.1 ms in 4.41.1. With 64
  clients on 32 cores that also run the clients, the tail is where the extra write work shows most.

## pgbench: both servers and PostgreSQL over the wire

`docs/benchmarks/bench-engines.sh` (sha256 `d99e58d5ba9056700f6a808e5ea4bb3527e35708f426d85f1a6772494fc655e6`), with `pgbench` and `psql` from the
`postgres:18.4-bookworm` image (`sha256:9479eac93922431e8a512b016d7362ae264f4ba139f840891b51f02931e450db`),
8 s per cell, each engine on its own port of the same host. Run twice, the two Nano servers in
opposite order; a cell is the mean of the two runs and `per run` is the 4.42.0 / 4.41.1 ratio of
each. The server binaries are the two measured above. The indexed point read is
`SELECT abalance FROM t50 WHERE aid = :aid` on a 50,000-row table that has an index on `aid` and no
primary key.

**SELECT 1** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 19,625 | 25,630 | 24,244 | 0.95 | 0.96, 0.94 |
| c=8 | 138,451 | 120,059 | 117,906 | 0.98 | 0.99, 0.98 |
| c=16 | 212,631 | 198,655 | 201,062 | 1.01 | 1.01, 1.02 |
| c=32 | 297,342 | 244,137 | 233,222 | 0.96 | 0.96, 0.95 |
| c=64 | 325,344 | 258,193 | 268,468 | 1.04 | 1.06, 1.02 |

**bulk-load COPY (rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 10000 | 92 | 51 | 50 | 0.98 | 1.04, 0.93 |
| 50000 | 78 | 110 | 110 | 1.00 | 1.05, 0.96 |
| 100000 | 114 | 169 | 181 | 1.07 | 1.15, 1.01 |

**indexed point-read (50k rows) [proto=simple]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 11,070 | 14,353 | 14,085 | 0.98 | 0.99, 0.97 |
| c=8 | 81,257 | 77,148 | 72,528 | 0.94 | 0.94, 0.94 |
| c=16 | 136,149 | 135,437 | 125,838 | 0.93 | 0.93, 0.93 |
| c=32 | 183,908 | 179,529 | 162,518 | 0.91 | 0.93, 0.88 |
| c=64 | 232,537 | 179,118 | 176,266 | 0.98 | 0.99, 0.98 |

**indexed point-read (50k rows) [proto=extended]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 10,095 | 9,545 | 8,799 | 0.92 | 0.93, 0.92 |
| c=8 | 74,969 | 51,342 | 46,730 | 0.91 | 0.89, 0.93 |
| c=16 | 123,653 | 92,371 | 83,345 | 0.90 | 0.90, 0.90 |
| c=32 | 171,638 | 118,938 | 107,363 | 0.90 | 0.90, 0.91 |
| c=64 | 206,704 | 119,753 | 108,395 | 0.91 | 0.91, 0.90 |

**indexed point-read (50k rows) [proto=prepared]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 17,896 | 15,870 | 13,558 | 0.85 | 0.86, 0.84 |
| c=8 | 131,514 | 83,088 | 70,771 | 0.85 | 0.86, 0.84 |
| c=16 | 194,922 | 145,157 | 127,110 | 0.88 | 0.89, 0.86 |
| c=32 | 280,694 | 198,590 | 170,617 | 0.86 | 0.86, 0.86 |
| c=64 | 305,636 | 203,252 | 174,824 | 0.86 | 0.87, 0.85 |

**DROP TABLE (100k rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 100k | 61 | 131 | 136 | 1.04 | 1.08, 1.00 |

## What is slower than 4.41.1, and why

Measured causes first, then what was not traced.

- **One-row `UPDATE` and `DELETE` (wire 0.46–0.71; pg35 `UPDATE point` 3.8x, `DELETE point` 2.3x).**
  A write on the primary-key fast path now puts the row's history and its snapshot record into the
  same write group as the row, so that `AS OF` is correct after it. Checked directly: after a
  fast-path `DELETE`, a query `AS OF` a moment before it does not find the row in 4.41.1 and finds
  it in 4.42.0. A store that does not use `AS OF`, `VERSIONS BETWEEN` or branches from the past can
  set `[storage] time_travel_enabled = false`; an earlier build of this release measured 0.86 /
  0.96 / 0.95 of 4.41.1 at 1 / 16 / 64 clients that way (CHANGELOG, *primary-key writes over the
  wire*). That setting was not remeasured on the final build.
- **`INSERT single` (4.0x in pg35).** The median insert is 1.3x slower (10.7 µs against 8.1 µs in an
  embedded probe of the same statement). The rest of the 4.0x is one pause: an in-memory database
  writes the records of the snapshots it evicts from memory once per 1,000 commits, on the
  committing thread, and that takes about 5 ms; one such pass falls inside the 300 timed inserts.
  A long-running writer on an in-memory database sees that pause every 1,000 commits.
- **`DROP TABLE` 2.4x, and `CREATE TABLE` 2.2x (the category drops what it creates).** This release
  adds dependency discovery to `DROP TABLE`; it reads every table's constraint record three times.
  The part of the regression that grew with the number of earlier drops was fixed (6.0x before).
- **Not traced to a cause:** `TRUNCATE` 1.7x, `CREATE INDEX` 1.6x, `REFRESH MATERIALIZED VIEW` 1.6x,
  `SET`/`SHOW`/`RESET` 1.6x, `UPSERT` 1.5x, multi-row `INSERT` 1.5x, prepared statements 1.5x,
  `CREATE`/`DROP VIEW` 1.4x, 4-table join 1.4x, `UPDATE` with a subquery 1.4x, transaction control
  1.3x, `UNION` 1.2x, string functions 1.2x, and the 0.85–0.92 of the pgbench indexed read in the
  extended and prepared protocols. This release makes every statement atomic (its own transaction,
  one write group, a schema lock) and moves statements off the connection's worker thread; that is
  the likely common cost, and it has not been measured category by category.

In absolute terms the slower embedded write and schema categories remain 22 to 1,293 times faster
than the PostgreSQL reference in this deployment; the 4-table join is slower than PostgreSQL in
both releases. The wire numbers are the ones to weigh if single-row write throughput is what you
upgrade for.

## How to reproduce

PostgreSQL 18.4 must listen on `127.0.0.1:25433` with database `benchdb` and user `bench`
(the header of `benches/public/pg35_run.py` gives the container command). Then, in a checkout of
this release:

```
benches/public/release_bench.sh v4.41.1 out            # pg35 + wire; writes out/RESULTS.md
PROTOCOLS="simple extended prepared" DUR=8 CLIENTS="1 8 16 32 64" \
  docs/benchmarks/bench-engines.sh 4.41.1:out/bin/v4.41.1/heliosdb-nano 4.42.0:out/bin/v4.42.0/heliosdb-nano
```

`release_bench.sh` clones `v4.41.1` out of the repository, copies the five pg35 harness files over
the clone (test files only), builds both sides with `--release --locked`, runs pg35 for 2
alternating rounds and the wire benchmark for 4, and records the commit, the toolchain and the
sha256 of every binary it measured. It needs cargo, git and python3; `bench-engines.sh` also needs
Docker for the `pgbench` client. On the 32-core host above the first command takes about 70
minutes, of which 15 are the two builds, and the second about 10 minutes per run.

The output directory keeps what a reader needs to check a number: `pg35/arms/*/strict-receipt.json`
(the harness's own receipt of every run), `pg35/build/*/build-receipt.json` and
`source-manifest.json`, `pg35/postgres-preflight.json` (the identity and settings of the PostgreSQL
measured), `wire/results.jsonl` (every wire run with its latency percentiles) and
`wire/provenance.txt`.
