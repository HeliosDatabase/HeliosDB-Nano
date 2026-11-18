# HeliosDB-Nano 4.42.0 against 4.41.1: the release benchmark

The 4.42.0 release build measured against the 4.41.1 release build, on one host, on 2026-11-17, with
the commands in [`REPRODUCING.md`](REPRODUCING.md). Every table is copied from the output of those
commands, and every number in the text is computed from it; nothing was edited by hand. The same
run as one JSON document: [`results/release-4.42.0/summary.json`](results/release-4.42.0/summary.json).

| part | what it measures | how |
|---|---|---|
| pg35 | 35 SQL categories, Nano embedded and in memory, beside PostgreSQL 18.4 | `benches/public/pg35_run.py` |
| wire | a one-row `UPDATE` by primary key against a running server, prepared and as text, 1 to 64 clients | `benches/public/wire_pk_ab.sh` |
| pgbench | `SELECT 1`, `COPY`, an indexed point read in three protocols, `DROP TABLE`, against PostgreSQL and both servers | `docs/benchmarks/bench-engines.sh` |

`v4.42.0-2a5d741a` is 4.42.0 as first benchmarked on 2026-11-17, before the second performance pass (commit `2a5d741a`); `v4.42.0` is the release.

## What the numbers say

- **Against PostgreSQL 18.4** (pg35): 4.42.0 is faster in 34 of the 35 categories
  and slower in 1 (4-table JOIN); 4.41.1 in
  34 (4-table JOIN slower). Nano runs in the test
  process and in memory, PostgreSQL is a server over TCP that syncs every commit: compare releases by
  the ratio between them, not by `x PG`.
- **Against 4.41.1** (pg35, time per statement): within 15% either way in 20 categories;
  slower in 11 (INSERT single 3.84x, UPDATE point 3.50x, DELETE point 2.60x, DROP TABLE 1.53x, REFRESH MATVIEW 1.52x, CREATE TABLE 1.52x, 4-table JOIN 1.41x, TRUNCATE 1.35x, Prepared stmts 1.24x, DISTINCT 1.18x, INSERT multi-row 1.18x); faster in 4 (INSERT..SELECT 0.14x, ALTER TABLE 0.28x, Transaction ctl 0.73x, EXISTS subquery 0.83x).
- **What the second pass changed** (`v4.42.0` against `v4.42.0-2a5d741a`, same run): faster by 13% or more in
  13 categories (ALTER TABLE 0.38x, Transaction ctl 0.56x, UPSERT 0.61x, DROP TABLE 0.67x, CREATE INDEX 0.68x, CREATE TABLE 0.72x, CREATE/DROP VIEW 0.76x, INNER JOIN 0.81x, UPDATE+subquery 0.83x, INSERT multi-row 0.84x, SET/SHOW/RESET 0.84x, Aggregation 0.86x, TRUNCATE 0.87x); slower by 15% or more in 0.
  The rest are within the noise of two rounds.
- **One-row `UPDATE` over the wire** runs at 0.47–0.72 of 4.41.1 prepared
  (lowest at 64 clients, highest at 1) and
  0.46–0.64 as text (lowest at 64, highest at
  8). Each such commit records the row's history and its snapshot:
  seven keys where 4.41.1 wrote one. See *What is slower, and why*.
- **With `[storage] time_travel_enabled = false`** the same server writes what 4.41.1 wrote and
  runs the one-row `UPDATE` at 0.93–0.99 of 4.41.1 (1, 16 and 64 clients, both protocols).
- **pgbench**: the indexed point read runs at 0.90–0.98 of 4.41.1 as
  text, 0.88–0.92 in the extended protocol and
  0.83–0.89 prepared; `SELECT 1`, `COPY` and `DROP TABLE` are in the
  tables below.

## What was measured

| | |
|---|---|
| date (UTC) | 2026-11-18T00:52:12Z |
| platform | Linux x86_64, 32 logical CPUs, at least 64 GiB of memory |
| toolchain | rustc 1.100.0 (0ef1bc386 2026-11-09); cargo 1.100.0 (3acc79c0c 2026-11-05) |
| v4.42.0 | commit `c65978604c3add569043e281077c8b77d90667fe`, 2 uncommitted file(s) |
| v4.41.1 | commit `4af19e1c447b8b9f8a621a984e8cc0558195bf3b`, pg35 harness files copied from v4.42.0 |
| `bin/v4.42.0/heliosdb-nano` | sha256 `40ee9a9619f6979f6e44e1b57887ae87cd1e375a8c2d04760cad234bc9181a8d` |
| `bin/v4.41.1/heliosdb-nano` | sha256 `956bc158fd90f677724a2f4133e1594a1cdaa75154ff44d38817ef5beddef947` |
| `bin/wire_pk_bench` | sha256 `e303016e9f70ede4e9acf35b842be259b73bf610cd5a14cd87dbc92c2eba970f` |

Both server binaries and both pg35 harnesses are built with `--release --locked` (LTO, one codegen unit).

The 4.42.0 server binary is the one the release gates built and tested from the same commit. The
host runs other workloads; each run waits for the 1-minute load to fall below 6 and the releases
alternate, so the ratios between them are steadier than the absolute numbers.

## pg35: 35 SQL categories beside PostgreSQL 18.4

pg35: 35 categories, 300 timed iterations each (after 2 warm-ups), median of 2 round(s).

Nano runs embedded and in memory inside the test process. PostgreSQL is `PostgreSQL 18.4 (Debian 18.4-1.pgdg12+1)`, a server reached over loopback TCP with `fsync=on`, `synchronous_commit=on`. Times are per statement; `x PG` is PostgreSQL's time divided by Nano's.

| category | PostgreSQL | v4.41.1 | x PG | v4.42.0-2a5d741a | x PG | v4.42.0 | x PG | v4.42.0-2a5d741a / v4.41.1 (time) | v4.42.0 / v4.41.1 (time) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| CREATE TABLE | 35.69 ms | 168.2 us | 212.2 | 355.5 us | 100.4 | 255.6 us | 139.6 | 2.11 | 1.52 |
| CREATE INDEX | 34.01 ms | 316.4 us | 107.5 | 480.6 us | 70.8 | 325.3 us | 104.5 | 1.52 | 1.03 |
| ALTER TABLE | 29.67 ms | 951.9 us | 31.2 | 700.6 us | 42.3 | 269.4 us | 110.1 | 0.74 | 0.28 |
| DROP TABLE | 14.54 ms | 153.9 us | 94.5 | 352.0 us | 41.3 | 234.9 us | 61.9 | 2.29 | 1.53 |
| CREATE/DROP VIEW | 28.20 ms | 125.8 us | 224.2 | 175.3 us | 160.9 | 132.5 us | 212.8 | 1.39 | 1.05 |
| REFRESH MATVIEW | 16.60 ms | 542.9 us | 30.6 | 837.0 us | 19.8 | 826.4 us | 20.1 | 1.54 | 1.52 |
| TRUNCATE | 86.39 ms | 167.2 us | 516.6 | 260.9 us | 331.1 | 225.8 us | 382.6 | 1.56 | 1.35 |
| INSERT single | 13.74 ms | 7.01 us | 1,960.2 | 25.15 us | 546.5 | 26.90 us | 510.9 | 3.59 | 3.84 |
| INSERT multi-row | 14.26 ms | 161.4 us | 88.4 | 227.1 us | 62.8 | 189.8 us | 75.2 | 1.41 | 1.18 |
| INSERT..SELECT | 14.48 ms | 1.36 ms | 10.6 | 211.0 us | 68.6 | 187.5 us | 77.2 | 0.15 | 0.14 |
| UPDATE point | 14.13 ms | 9.66 us | 1,462.3 | 36.36 us | 388.7 | 33.81 us | 418.0 | 3.76 | 3.50 |
| DELETE point | 13.90 ms | 4.53 us | 3,067.7 | 12.35 us | 1,125.5 | 11.80 us | 1,177.8 | 2.73 | 2.60 |
| UPSERT | 14.27 ms | 62.42 us | 228.7 | 100.2 us | 142.4 | 61.22 us | 233.1 | 1.61 | 0.98 |
| UPDATE+subquery | 13.97 ms | 157.7 us | 88.6 | 192.9 us | 72.4 | 159.8 us | 87.4 | 1.22 | 1.01 |
| Point lookup | 202.6 us | 2.98 us | 67.9 | 3.68 us | 55.0 | 3.21 us | 63.2 | 1.23 | 1.07 |
| Full scan+filter | 262.7 us | 30.76 us | 8.5 | 31.41 us | 8.4 | 28.56 us | 9.2 | 1.02 | 0.93 |
| Aggregation | 276.7 us | 1.08 us | 255.3 | 1.34 us | 207.2 | 1.15 us | 239.6 | 1.23 | 1.07 |
| INNER JOIN | 250.6 us | 24.83 us | 10.1 | 32.94 us | 7.6 | 26.65 us | 9.4 | 1.33 | 1.07 |
| LEFT JOIN | 256.9 us | 18.99 us | 13.5 | 23.28 us | 11.0 | 20.35 us | 12.6 | 1.23 | 1.07 |
| 4-table JOIN | 505.3 us | 646.1 us | 0.8 | 940.9 us | 0.5 | 909.9 us | 0.6 | 1.46 | 1.41 |
| Scalar subquery | 363.5 us | 2.76 us | 131.8 | 3.06 us | 119.0 | 2.71 us | 134.3 | 1.11 | 0.98 |
| EXISTS subquery | 473.3 us | 5.79 us | 81.8 | 5.25 us | 90.1 | 4.80 us | 98.6 | 0.91 | 0.83 |
| IN subquery | 408.3 us | 6.07 us | 67.2 | 6.06 us | 67.4 | 6.01 us | 67.9 | 1.00 | 0.99 |
| CTE | 482.9 us | 7.21 us | 67.0 | 7.13 us | 67.7 | 6.32 us | 76.4 | 0.99 | 0.88 |
| Recursive CTE | 323.5 us | 3.45 us | 93.7 | 3.69 us | 87.8 | 3.35 us | 96.5 | 1.07 | 0.97 |
| Window funcs | 521.8 us | 22.53 us | 23.2 | 20.42 us | 25.6 | 19.69 us | 26.5 | 0.91 | 0.87 |
| UNION | 332.0 us | 9.10 us | 36.5 | 8.53 us | 38.9 | 8.80 us | 37.7 | 0.94 | 0.97 |
| DISTINCT | 238.4 us | 0.80 us | 296.5 | 0.93 us | 257.6 | 0.95 us | 252.1 | 1.15 | 1.18 |
| ORDER+LIMIT | 286.0 us | 95.35 us | 3.0 | 97.36 us | 2.9 | 90.72 us | 3.2 | 1.02 | 0.95 |
| CASE expr | 256.6 us | 15.58 us | 16.5 | 16.04 us | 16.0 | 15.98 us | 16.1 | 1.03 | 1.03 |
| LIKE/BETWEEN/IN | 274.4 us | 12.39 us | 22.2 | 11.44 us | 24.0 | 10.77 us | 25.5 | 0.92 | 0.87 |
| String ops | 271.8 us | 14.08 us | 19.3 | 12.81 us | 21.2 | 12.62 us | 21.5 | 0.91 | 0.90 |
| Transaction ctl | 14.39 ms | 50.06 us | 287.5 | 64.40 us | 223.5 | 36.31 us | 396.4 | 1.29 | 0.73 |
| Prepared stmts | 452.7 us | 3.42 us | 132.3 | 4.43 us | 102.3 | 4.25 us | 106.5 | 1.29 | 1.24 |
| SET/SHOW/RESET | 381.2 us | 2.45 us | 155.7 | 2.93 us | 130.3 | 2.45 us | 155.7 | 1.19 | 1.00 |

- `v4.41.1`: faster than PostgreSQL in 34 of 35 categories.
- `v4.42.0-2a5d741a`: faster than PostgreSQL in 34 of 35 categories.
- `v4.42.0`: faster than PostgreSQL in 34 of 35 categories.

| run | load before | load after |
|---|---:|---:|
| v4_41_1-r1 | 2.32 | 2.8 |
| v4_42_0-2a5d741a-r1 | 2.8 | 2.2 |
| v4_42_0-r1 | 2.2 | 2.18 |
| v4_42_0-r2 | 2.18 | 2.64 |
| v4_42_0-2a5d741a-r2 | 2.64 | 2.33 |
| v4_41_1-r2 | 2.33 | 2.8 |

The harness checks every statement's result against an independent model on both engines; a run
with a wrong result produces no number. A cell is the mean of 300 timed statements; the table shows
the median of the rounds, and `summary.json` has every round.

## Wire: one-row UPDATE by primary key against a running server

`extended` is a prepared statement bound per execution; `simple` is the statement sent as text.
Client and server share the host. Each client has its own connection and its own rows.

Median of 4 round(s); ratio is ops/s against `v4.41.1`.

| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |
|---|---|---:|---|---:|---:|---:|---:|---:|---|
| update | extended | 1 | v4.41.1 | 13,614 | 1.00 | 71 | 92 | 0 | 4.88/15.7/16.32/14.96 |
| update | extended | 1 | v4.42.0 | 9,796 | 0.72 | 100 | 127 | 0 | 13.03/15.44/15.18/15.77 |
| update | extended | 2 | v4.41.1 | 20,474 | 1.00 | 92 | 120 | 0 | 4.12/12.18/11.35/10.79 |
| update | extended | 2 | v4.42.0 | 12,180 | 0.59 | 160 | 222 | 0 | 10.0/10.76/11.45/11.05 |
| update | extended | 4 | v4.41.1 | 37,312 | 1.00 | 98 | 178 | 0 | 3.83/9.16/9.15/8.51 |
| update | extended | 4 | v4.42.0 | 24,396 | 0.65 | 153 | 277 | 0 | 7.61/8.49/8.24/8.69 |
| update | extended | 8 | v4.41.1 | 62,102 | 1.00 | 120 | 206 | 0 | 4.13/7.5/7.83/7.4 |
| update | extended | 8 | v4.42.0 | 41,550 | 0.67 | 182 | 360 | 0 | 6.7/7.56/7.27/7.6 |
| update | extended | 16 | v4.41.1 | 106,839 | 1.00 | 139 | 306 | 0 | 5.77/7.77/7.71/9.55 |
| update | extended | 16 | v4.42.0 | 66,777 | 0.63 | 224 | 629 | 0 | 7.38/8.42/7.49/8.1 |
| update | extended | 32 | v4.41.1 | 139,686 | 1.00 | 201 | 658 | 0 | 9.52/10.05/10.53/11.35 |
| update | extended | 32 | v4.42.0 | 69,583 | 0.50 | 395 | 1,097 | 0 | 10.74/11.75/10.48/11.51 |
| update | extended | 64 | v4.41.1 | 143,176 | 1.00 | 404 | 1,120 | 0 | 14.1/13.7/13.9/14.72 |
| update | extended | 64 | v4.42.0 | 67,860 | 0.47 | 826 | 5,898 | 0 | 14.19/15.1/13.0/10.87 |
| update | simple | 1 | v4.41.1 | 12,877 | 1.00 | 75 | 95 | 0 | 16.99/16.01/15.7/16.44 |
| update | simple | 1 | v4.42.0 | 7,733 | 0.60 | 124 | 197 | 0 | 15.01/17.15/14.97/10.19 |
| update | simple | 2 | v4.41.1 | 18,972 | 1.00 | 100 | 128 | 0 | 11.86/11.28/10.81/11.68 |
| update | simple | 2 | v4.42.0 | 10,079 | 0.53 | 194 | 272 | 0 | 10.81/12.26/10.79/7.49 |
| update | simple | 4 | v4.41.1 | 37,511 | 1.00 | 97 | 178 | 0 | 9.27/8.74/8.67/8.65 |
| update | simple | 4 | v4.42.0 | 21,932 | 0.58 | 170 | 314 | 0 | 8.24/9.49/8.37/6.37 |
| update | simple | 8 | v4.41.1 | 63,241 | 1.00 | 118 | 200 | 0 | 7.96/7.4/7.26/7.56 |
| update | simple | 8 | v4.42.0 | 40,683 | 0.64 | 186 | 420 | 0 | 7.43/8.51/7.63/6.38 |
| update | simple | 16 | v4.41.1 | 109,761 | 1.00 | 135 | 300 | 0 | 8.84/9.03/7.45/7.91 |
| update | simple | 16 | v4.42.0 | 64,822 | 0.59 | 228 | 657 | 0 | 7.96/9.27/9.61/7.82 |
| update | simple | 32 | v4.41.1 | 144,717 | 1.00 | 196 | 652 | 0 | 10.85/10.82/9.15/9.73 |
| update | simple | 32 | v4.42.0 | 72,379 | 0.50 | 388 | 1,028 | 0 | 10.08/10.89/11.52/10.12 |
| update | simple | 64 | v4.41.1 | 147,069 | 1.00 | 396 | 1,085 | 0 | 13.18/13.39/12.94/11.22 |
| update | simple | 64 | v4.42.0 | 68,072 | 0.46 | 836 | 4,932 | 0 | 14.1/14.47/15.19/14.05 |

How to read it: `load` is the host's 1-minute load when each round of a cell began; a 1-client cell
begins right after a 64-client cell, and the load average takes minutes to fall.

### With time travel off

The same 4.42.0 binary (sha256 `40ee9a9619f6979f6e44e1b57887ae87cd1e375a8c2d04760cad234bc9181a8d`), started with a configuration file
that holds only `[storage] time_travel_enabled = false`, against 4.41.1: 1, 16 and 64 clients, 2
rounds. A store that does not use `AS OF`, `VERSIONS BETWEEN` or branches from the past loses nothing
by it.

Median of 2 round(s); ratio is ops/s against `v4.41.1`.

| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |
|---|---|---:|---|---:|---:|---:|---:|---:|---|
| update | extended | 1 | v4.41.1 | 12,789 | 1.00 | 75 | 96 | 0 | 5.69/14.83 |
| update | extended | 1 | v4.42.0-tt-off | 12,670 | 0.99 | 78 | 98 | 0 | 12.94/14.21 |
| update | extended | 16 | v4.41.1 | 107,011 | 1.00 | 138 | 307 | 0 | 4.63/11.0 |
| update | extended | 16 | v4.42.0-tt-off | 101,555 | 0.95 | 150 | 292 | 0 | 9.69/10.98 |
| update | extended | 64 | v4.41.1 | 141,618 | 1.00 | 412 | 1,092 | 0 | 8.49/12.59 |
| update | extended | 64 | v4.42.0-tt-off | 132,110 | 0.93 | 440 | 1,148 | 0 | 11.03/13.47 |
| update | simple | 1 | v4.41.1 | 12,582 | 1.00 | 76 | 97 | 0 | 12.65/15.57 |
| update | simple | 1 | v4.42.0-tt-off | 11,879 | 0.94 | 82 | 100 | 0 | 15.06/16.24 |
| update | simple | 16 | v4.41.1 | 109,726 | 1.00 | 136 | 297 | 0 | 9.93/11.65 |
| update | simple | 16 | v4.42.0-tt-off | 104,176 | 0.95 | 146 | 285 | 0 | 11.21/12.2 |
| update | simple | 64 | v4.41.1 | 147,161 | 1.00 | 397 | 1,068 | 0 | 12.74/12.11 |
| update | simple | 64 | v4.42.0-tt-off | 139,812 | 0.95 | 420 | 1,099 | 0 | 12.34/12.84 |

## pgbench: both servers and PostgreSQL over the wire

`docs/benchmarks/bench-engines.sh` (sha256 `d99e58d5ba9056700f6a808e5ea4bb3527e35708f426d85f1a6772494fc655e6`), with `pgbench` and `psql` from the
`postgres:18.4-bookworm` image (`sha256:9479eac93922431e8a512b016d7362ae264f4ba139f840891b51f02931e450db`), 8 s per cell, each engine on its own port of the same
host. Run twice, the two Nano servers in opposite order; a cell is the mean of the two runs and
`per run` is the 4.42.0 / 4.41.1 ratio of each. The indexed point read is
`SELECT abalance FROM t50 WHERE aid = :aid` on a 50,000-row table that has an index on `aid` and no
primary key.

**SELECT 1** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 19,300 | 26,010 | 25,673 | 0.99 | 1.01, 0.96 |
| c=8 | 140,399 | 120,543 | 122,248 | 1.01 | 1.00, 1.03 |
| c=16 | 213,865 | 201,850 | 192,754 | 0.95 | 0.95, 0.96 |
| c=32 | 291,572 | 232,447 | 251,163 | 1.08 | 1.07, 1.09 |
| c=64 | 326,239 | 264,373 | 248,195 | 0.94 | 0.93, 0.95 |

**bulk-load COPY (rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 10000 | 66 | 48 | 51 | 1.05 | 1.04, 1.06 |
| 50000 | 81 | 107 | 104 | 0.98 | 0.96, 0.99 |
| 100000 | 117 | 168 | 177 | 1.06 | 1.01, 1.11 |

**indexed point-read (50k rows) [proto=simple]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 10,909 | 14,150 | 13,327 | 0.94 | 0.97, 0.91 |
| c=8 | 80,995 | 77,574 | 70,154 | 0.90 | 0.90, 0.91 |
| c=16 | 133,131 | 135,459 | 126,711 | 0.94 | 0.93, 0.94 |
| c=32 | 189,465 | 178,589 | 161,549 | 0.90 | 0.88, 0.93 |
| c=64 | 224,615 | 174,756 | 170,773 | 0.98 | 0.94, 1.02 |

**indexed point-read (50k rows) [proto=extended]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 10,166 | 9,552 | 8,801 | 0.92 | 0.91, 0.93 |
| c=8 | 74,252 | 51,458 | 47,248 | 0.92 | 0.90, 0.94 |
| c=16 | 124,241 | 93,187 | 85,474 | 0.92 | 0.90, 0.94 |
| c=32 | 173,757 | 121,647 | 106,672 | 0.88 | 0.87, 0.89 |
| c=64 | 200,373 | 122,153 | 109,050 | 0.89 | 0.88, 0.91 |

**indexed point-read (50k rows) [proto=prepared]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 17,447 | 15,694 | 13,909 | 0.89 | 0.88, 0.89 |
| c=8 | 128,511 | 82,712 | 72,381 | 0.88 | 0.85, 0.90 |
| c=16 | 199,959 | 148,602 | 122,636 | 0.83 | 0.81, 0.84 |
| c=32 | 271,921 | 196,369 | 172,011 | 0.88 | 0.88, 0.87 |
| c=64 | 308,433 | 206,175 | 173,285 | 0.84 | 0.81, 0.87 |

**DROP TABLE (100k rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 100k | 54 | 134 | 147 | 1.10 | 1.12, 1.07 |

## What is slower than 4.41.1, and why

- **One-row writes record history.** A one-row `UPDATE` or `DELETE` writes the row, its version and
  its version-index event, after checking the row's newest version (an iterator); on disk the commit
  also writes its snapshot record and that record's three index keys: seven keys where 4.41.1 wrote
  one. Profiled in process on disk, a prepared one-row `UPDATE` takes 33 µs against 11 µs in 4.41.1:
  the RocksDB write 14.5 µs against 6.3, the newest-version check 5.2, the snapshot record 2.1. Over
  the wire a server's connections then wait for RocksDB's write group, which is why the ratio falls
  as clients are added. It is what makes `AS OF` right after a fast-path write (4.41.1 lost a row
  deleted on its fast path from every `AS OF` before the delete); `time_travel_enabled = false` gives
  the 4.41.1 write path back to a store that does not need it.
- **An in-memory store pays for the history it keeps past its newest snapshots.** Beyond 2,000
  snapshots in memory, the commit that crosses the limit writes the records of the oldest 1,000
  (four keys each) so `AS OF` still reaches them: about 5 ms once per 1,000 commits. pg35 runs 302
  statements per category, so a category that commits sees that pause in roughly one run in three,
  which is most of the spread between rounds of the one-row write categories.
- **The rest** of the categories still slower than 4.41.1 were profiled; the changes that came out of
  it are listed in the changelog (*a second pass over what 4.42.0 added to a statement*). What those
  categories still have is the work every statement now does to be atomic: its own transaction, one
  write group, the layout epochs of the tables it reads.

## How to reproduce

[`REPRODUCING.md`](REPRODUCING.md) has the requirements, the commands and how to check that you
measured the same binaries and reference. In short, with PostgreSQL 18.4 on `127.0.0.1:25433`:

```
benches/public/release_bench.sh v4.41.1 out
PROTOCOLS="simple extended prepared" DUR=8 CLIENTS="1 8 16 32 64" \
  docs/benchmarks/bench-engines.sh 4.41.1:out/bin/v4.41.1/heliosdb-nano 4.42.0:out/bin/v4.42.0/heliosdb-nano
benches/public/export_results.py out > summary.json
```
