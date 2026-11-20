# HeliosDB-Nano 4.42.0 against 4.41.1: the release benchmark

The 4.42.0 release build measured against the 4.41.1 release build, on one host, on 2026-11-20, with
the commands in [`REPRODUCING.md`](REPRODUCING.md). Every table is copied from the output of those
commands, and every number in the text is computed from it; nothing was edited by hand. The same
run as one JSON document: [`results/release-4.42.0/summary.json`](results/release-4.42.0/summary.json).

| part | what it measures | how |
|---|---|---|
| pg35 | 35 SQL categories, Nano embedded and in memory, beside PostgreSQL 18.4 | `benches/public/pg35_run.py` |
| wire | a one-row `UPDATE` by primary key against a running server, prepared and as text, 1 to 64 clients | `benches/public/wire_pk_ab.sh` |
| pgbench | `SELECT 1`, `COPY`, an indexed point read in three protocols, `DROP TABLE`, against PostgreSQL and both servers | `docs/benchmarks/bench-engines.sh` |

`v4.42.0-2a5d741a` is 4.42.0 as first benchmarked on 2026-11-17, before the performance passes that followed (the commit then numbered `2a5d741a`, `255b19f9` in the published history); `v4.42.0` is the release.

## What the numbers say

- **Against PostgreSQL 18.4** (pg35): 4.42.0 is faster in all 35 categories; 4.41.1 is faster in
  34 of the 35 categories and slower in 1 (4-table JOIN). Nano runs in the test
  process and in memory, PostgreSQL is a server over TCP that syncs every commit: compare releases by
  the ratio between them, not by `x PG`.
- **Against 4.41.1** (pg35, time per statement): within 15% either way in 15 categories;
  slower in 14 (UPDATE point 3.58x, INSERT single 3.19x, DELETE point 3.09x, TRUNCATE 1.46x, SET/SHOW/RESET 1.44x, REFRESH MATVIEW 1.38x, DISTINCT 1.33x, UNION 1.33x, Prepared stmts 1.33x, Aggregation 1.31x, Scalar subquery 1.26x, CREATE TABLE 1.25x, CREATE INDEX 1.22x, INSERT multi-row 1.17x); faster in 6 (4-table JOIN 0.08x, INSERT..SELECT 0.13x, ALTER TABLE 0.31x, DROP TABLE 0.70x, Transaction ctl 0.82x, LEFT JOIN 0.86x).
- **What the performance passes changed** (`v4.42.0` against `v4.42.0-2a5d741a`, same run): faster by 13% or more in
  11 categories (4-table JOIN 0.07x, DROP TABLE 0.30x, ALTER TABLE 0.42x, Transaction ctl 0.56x, CREATE TABLE 0.58x, UPSERT 0.64x, CREATE/DROP VIEW 0.72x, UPDATE+subquery 0.76x, CREATE INDEX 0.76x, ORDER+LIMIT 0.77x, INNER JOIN 0.86x); slower by 15% or more in 1 (Aggregation 1.21x).
  The rest are within the noise of two rounds.
- **One-row `UPDATE` over the wire** runs at 0.48–0.74 of 4.41.1 prepared
  (lowest at 64 clients, highest at 1) and
  0.45–0.64 as text (lowest at 64, highest at
  8). Each such commit records the row's history and its snapshot:
  seven keys where 4.41.1 wrote one. See *What is slower, and why*.
- **Against the baseline** (the best time each category has reached in a release, best round of each):
  4.42.0 sets a new baseline in 9 of the 35 categories (ALTER TABLE, DROP TABLE, CREATE/DROP VIEW, INSERT..SELECT, UPSERT, INNER JOIN, LEFT JOIN, 4-table JOIN, Transaction ctl).
  In the other 26 the baseline stays with 4.41.1: 4.42.0 is within 15% of it in
  12 and more than 15% slower in 14 (INSERT single 3.80x, UPDATE point 3.57x, DELETE point 3.07x, TRUNCATE 1.41x, SET/SHOW/RESET 1.39x, REFRESH MATVIEW 1.39x, DISTINCT 1.38x, Aggregation 1.35x, CREATE TABLE 1.34x, Prepared stmts 1.30x, INSERT multi-row 1.22x, Scalar subquery 1.18x, UPDATE+subquery 1.17x, Point lookup 1.15x).
- **With `[storage] time_travel_enabled = false`** the same server writes what 4.41.1 wrote and
  runs the one-row `UPDATE` at 0.89–0.94 of 4.41.1 (1, 16 and 64 clients, both protocols).
- **pgbench**: the indexed point read runs at 0.91–0.96 of 4.41.1 as
  text, 0.87–0.91 in the extended protocol and
  0.77–0.87 prepared; `SELECT 1`, `COPY` and `DROP TABLE` are in the
  tables below.

## What was measured

| | |
|---|---|
| date (UTC) | 2026-11-20T10:17:38Z |
| platform | Linux x86_64, 32 logical CPUs, at least 64 GiB of memory |
| toolchain | rustc 1.100.0 (0ef1bc386 2026-11-09); cargo 1.100.0 (3acc79c0c 2026-11-05) |
| v4.42.0 | commit `4784e7732c399d154038d8c762ba2db1152789f0`, 2 uncommitted file(s) |
| v4.41.1 | commit `4af19e1c447b8b9f8a621a984e8cc0558195bf3b`, pg35 harness files copied from v4.42.0 |
| `bin/v4.42.0/heliosdb-nano` | sha256 `37cf8d142df6c4a1455d7709c3fa05eebbb985f67e8f69ec9b395d8f80ff42b6` |
| `bin/v4.41.1/heliosdb-nano` | sha256 `956bc158fd90f677724a2f4133e1594a1cdaa75154ff44d38817ef5beddef947` |
| `bin/wire_pk_bench` | sha256 `1527d5115680ad2b456a18fc0fc876b0291e3b1f768809277badaaea8b9ac457` |

Both server binaries and both pg35 harnesses are built with `--release --locked` (LTO, one codegen unit).

The 4.42.0 server binary is the one the release gates built and tested from the same commit. The
host runs other workloads; each run waits for the 1-minute load to fall below 6 and the releases
alternate, so the ratios between them are steadier than the absolute numbers.

## pg35: 35 SQL categories beside PostgreSQL 18.4

pg35: 35 categories, 300 timed iterations each (after 2 warm-ups), median of 2 round(s).

Nano runs embedded and in memory inside the test process. PostgreSQL is `PostgreSQL 18.4 (Debian 18.4-1.pgdg12+1)`, a server reached over loopback TCP with `fsync=on`, `synchronous_commit=on`. Times are per statement; `x PG` is PostgreSQL's time divided by Nano's.

| category | PostgreSQL | v4.41.1 | x PG | v4.42.0-2a5d741a | x PG | v4.42.0 | x PG | v4.42.0-2a5d741a / v4.41.1 (time) | v4.42.0 / v4.41.1 (time) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| CREATE TABLE | 32.26 ms | 163.9 us | 196.8 | 350.1 us | 92.1 | 204.6 us | 157.6 | 2.14 | 1.25 |
| CREATE INDEX | 30.01 ms | 299.4 us | 100.2 | 480.9 us | 62.4 | 366.7 us | 81.8 | 1.61 | 1.22 |
| ALTER TABLE | 27.83 ms | 918.4 us | 30.3 | 665.8 us | 41.8 | 280.6 us | 99.2 | 0.72 | 0.31 |
| DROP TABLE | 13.22 ms | 158.9 us | 83.2 | 368.5 us | 35.9 | 110.9 us | 119.2 | 2.32 | 0.70 |
| CREATE/DROP VIEW | 26.70 ms | 125.9 us | 212.1 | 181.9 us | 146.8 | 131.7 us | 202.8 | 1.44 | 1.05 |
| REFRESH MATVIEW | 16.79 ms | 553.2 us | 30.4 | 813.7 us | 20.6 | 762.9 us | 22.0 | 1.47 | 1.38 |
| TRUNCATE | 85.53 ms | 164.2 us | 520.9 | 274.9 us | 311.2 | 240.0 us | 356.4 | 1.67 | 1.46 |
| INSERT single | 13.11 ms | 8.17 us | 1,605.1 | 27.65 us | 474.3 | 26.02 us | 503.9 | 3.38 | 3.19 |
| INSERT multi-row | 13.94 ms | 175.8 us | 79.3 | 226.7 us | 61.5 | 206.4 us | 67.5 | 1.29 | 1.17 |
| INSERT..SELECT | 13.46 ms | 1.31 ms | 10.2 | 190.4 us | 70.7 | 168.2 us | 80.0 | 0.14 | 0.13 |
| UPDATE point | 13.92 ms | 9.67 us | 1,440.0 | 35.68 us | 390.1 | 34.58 us | 402.6 | 3.69 | 3.58 |
| DELETE point | 14.19 ms | 4.40 us | 3,228.9 | 13.17 us | 1,077.6 | 13.57 us | 1,046.1 | 3.00 | 3.09 |
| UPSERT | 13.93 ms | 64.10 us | 217.3 | 101.0 us | 137.9 | 64.82 us | 214.9 | 1.58 | 1.01 |
| UPDATE+subquery | 13.62 ms | 136.8 us | 99.6 | 201.1 us | 67.7 | 152.2 us | 89.5 | 1.47 | 1.11 |
| Point lookup | 198.7 us | 2.82 us | 70.4 | 3.38 us | 58.8 | 3.21 us | 62.0 | 1.20 | 1.14 |
| Full scan+filter | 260.7 us | 27.41 us | 9.5 | 28.76 us | 9.1 | 27.13 us | 9.6 | 1.05 | 0.99 |
| Aggregation | 273.6 us | 1.10 us | 248.7 | 1.20 us | 228.9 | 1.45 us | 189.2 | 1.09 | 1.31 |
| INNER JOIN | 246.6 us | 28.05 us | 8.8 | 29.32 us | 8.4 | 25.30 us | 9.7 | 1.05 | 0.90 |
| LEFT JOIN | 253.0 us | 21.39 us | 11.8 | 21.03 us | 12.0 | 18.40 us | 13.7 | 0.98 | 0.86 |
| 4-table JOIN | 519.6 us | 687.3 us | 0.8 | 894.3 us | 0.6 | 58.22 us | 8.9 | 1.30 | 0.08 |
| Scalar subquery | 352.7 us | 2.58 us | 136.8 | 2.98 us | 118.3 | 3.25 us | 108.5 | 1.16 | 1.26 |
| EXISTS subquery | 467.6 us | 5.27 us | 88.8 | 5.36 us | 87.3 | 5.55 us | 84.2 | 1.02 | 1.05 |
| IN subquery | 398.6 us | 5.16 us | 77.2 | 6.19 us | 64.4 | 5.65 us | 70.6 | 1.20 | 1.09 |
| CTE | 467.1 us | 6.32 us | 73.9 | 7.06 us | 66.2 | 6.53 us | 71.5 | 1.12 | 1.03 |
| Recursive CTE | 313.2 us | 3.07 us | 101.9 | 3.31 us | 94.5 | 3.21 us | 97.7 | 1.08 | 1.04 |
| Window funcs | 504.2 us | 19.56 us | 25.8 | 20.20 us | 25.0 | 21.30 us | 23.7 | 1.03 | 1.09 |
| UNION | 332.3 us | 7.32 us | 45.4 | 8.97 us | 37.0 | 9.72 us | 34.2 | 1.23 | 1.33 |
| DISTINCT | 230.9 us | 0.71 us | 323.5 | 0.99 us | 232.3 | 0.95 us | 242.8 | 1.39 | 1.33 |
| ORDER+LIMIT | 286.7 us | 98.80 us | 2.9 | 124.4 us | 2.3 | 95.22 us | 3.0 | 1.26 | 0.96 |
| CASE expr | 261.5 us | 16.47 us | 15.9 | 16.31 us | 16.0 | 15.61 us | 16.7 | 0.99 | 0.95 |
| LIKE/BETWEEN/IN | 281.4 us | 10.50 us | 26.8 | 11.36 us | 24.8 | 10.46 us | 26.9 | 1.08 | 1.00 |
| String ops | 268.8 us | 13.32 us | 20.2 | 12.64 us | 21.3 | 13.10 us | 20.5 | 0.95 | 0.98 |
| Transaction ctl | 14.66 ms | 49.90 us | 293.8 | 73.27 us | 200.1 | 41.04 us | 357.2 | 1.47 | 0.82 |
| Prepared stmts | 450.9 us | 3.58 us | 125.8 | 4.18 us | 107.8 | 4.75 us | 94.9 | 1.17 | 1.33 |
| SET/SHOW/RESET | 351.8 us | 1.99 us | 176.6 | 2.96 us | 118.8 | 2.86 us | 122.9 | 1.49 | 1.44 |

- `v4.41.1`: faster than PostgreSQL in 34 of 35 categories.
- `v4.42.0-2a5d741a`: faster than PostgreSQL in 34 of 35 categories.
- `v4.42.0`: faster than PostgreSQL in 35 of 35 categories.

| run | load before | load after |
|---|---:|---:|
| v4_41_1-r1 | 2.29 | 2.44 |
| v4_42_0-2a5d741a-r1 | 2.44 | 2.28 |
| v4_42_0-r1 | 2.28 | 3.16 |
| v4_42_0-r2 | 3.16 | 3.4 |
| v4_42_0-2a5d741a-r2 | 3.4 | 2.6 |
| v4_41_1-r2 | 2.6 | 3.28 |

The harness checks every statement's result against an independent model on both engines; a run
with a wrong result produces no number. A cell is the mean of 300 timed statements; the table shows
the median of the rounds, and `summary.json` has every round.

Two rounds do not settle a 15% line everywhere. Pairing each round of 4.42.0 with each round of
4.41.1, 15 of the 35 categories fall on different sides of it depending on the pair
(CREATE INDEX 1.03–1.46, INSERT multi-row 1.07–1.28, UPDATE+subquery 1.01–1.23, Point lookup 1.02–1.26, INNER JOIN 0.83–0.99, LEFT JOIN 0.84–0.88, Scalar subquery 1.14–1.40, EXISTS subquery 0.87–1.29, IN subquery 0.91–1.32, CTE 0.88–1.23, Recursive CTE 0.90–1.21, UNION 1.14–1.52, DISTINCT 1.13–1.58, CASE expr 0.86–1.05, String ops 0.87–1.13); the other 20 are in the same class for
every pair. The baseline below compares best rounds for that reason.

## Against the baseline: the best time each category has reached

The baseline of a category is the best (lowest) time per statement a release has reached in it. A
release that reaches a new best sets the baseline; nothing else moves it, so a release that is
slower in a category is measured against the best the engine has done. The file is
[`benches/public/pg35_baseline.json`](../../benches/public/pg35_baseline.json); the rule and the
commands are in [`REPRODUCING.md`](REPRODUCING.md#6-compare-a-run-with-the-baseline).

A time here is the best round of a release, not the median: on a busy host noise only adds time.
Rounds counted: 4.41.1: 8 rounds, 4.42.0: 8 rounds; each release is one harness binary, measured with the same
statements and checks.

| Category | Baseline | Set by | Best of 4.41.1 | Best of 4.42.0 |
|---|---:|---|---:|---:|
| CREATE TABLE | 150 µs | 4.41.1 | 150 µs | 200 µs |
| CREATE INDEX | 270 µs | 4.41.1 | 270 µs | 306 µs |
| ALTER TABLE | 252 µs | 4.42.0 | 863 µs | 252 µs |
| DROP TABLE | 108 µs | 4.42.0 | 141 µs | 108 µs |
| CREATE/DROP VIEW | 116 µs | 4.42.0 | 117 µs | 116 µs |
| REFRESH MATVIEW | 531 µs | 4.41.1 | 531 µs | 736 µs |
| TRUNCATE | 153 µs | 4.41.1 | 153 µs | 215 µs |
| INSERT single | 6.54 µs | 4.41.1 | 6.54 µs | 24.9 µs |
| INSERT multi-row | 150 µs | 4.41.1 | 150 µs | 182 µs |
| INSERT..SELECT | 159 µs | 4.42.0 | 1.27 ms | 159 µs |
| UPDATE point | 9.18 µs | 4.41.1 | 9.18 µs | 32.8 µs |
| DELETE point | 3.93 µs | 4.41.1 | 3.93 µs | 12.1 µs |
| UPSERT | 60.7 µs | 4.42.0 | 61.0 µs | 60.7 µs |
| UPDATE+subquery | 124 µs | 4.41.1 | 124 µs | 145 µs |
| Point lookup | 2.66 µs | 4.41.1 | 2.66 µs | 3.06 µs |
| Full scan+filter | 26.0 µs | 4.41.1 | 26.0 µs | 26.7 µs |
| Aggregation | 0.99 µs | 4.41.1 | 0.99 µs | 1.33 µs |
| INNER JOIN | 22.5 µs | 4.42.0 | 22.8 µs | 22.5 µs |
| LEFT JOIN | 17.0 µs | 4.42.0 | 17.2 µs | 17.0 µs |
| 4-table JOIN | 51.9 µs | 4.42.0 | 570 µs | 51.9 µs |
| Scalar subquery | 2.43 µs | 4.41.1 | 2.43 µs | 2.87 µs |
| EXISTS subquery | 4.50 µs | 4.41.1 | 4.50 µs | 4.71 µs |
| IN subquery | 4.54 µs | 4.41.1 | 4.54 µs | 4.88 µs |
| CTE | 5.52 µs | 4.41.1 | 5.52 µs | 5.74 µs |
| Recursive CTE | 2.86 µs | 4.41.1 | 2.86 µs | 2.97 µs |
| Window funcs | 19.2 µs | 4.41.1 | 19.2 µs | 19.5 µs |
| UNION | 7.08 µs | 4.41.1 | 7.08 µs | 8.01 µs |
| DISTINCT | 0.64 µs | 4.41.1 | 0.64 µs | 0.88 µs |
| ORDER+LIMIT | 83.8 µs | 4.41.1 | 83.8 µs | 86.4 µs |
| CASE expr | 14.8 µs | 4.41.1 | 14.8 µs | 15.0 µs |
| LIKE/BETWEEN/IN | 9.99 µs | 4.41.1 | 9.99 µs | 10.3 µs |
| String ops | 11.8 µs | 4.41.1 | 11.8 µs | 13.0 µs |
| Transaction ctl | 37.6 µs | 4.42.0 | 47.0 µs | 37.6 µs |
| Prepared stmts | 3.20 µs | 4.41.1 | 3.20 µs | 4.17 µs |
| SET/SHOW/RESET | 1.81 µs | 4.41.1 | 1.81 µs | 2.52 µs |

- **New baselines set by 4.42.0** (9): ALTER TABLE (863.3 µs → 251.9 µs), DROP TABLE (140.9 µs → 108.5 µs), CREATE/DROP VIEW (116.7 µs → 116.4 µs), INSERT..SELECT (1.27 ms → 158.6 µs), UPSERT (61.0 µs → 60.7 µs), INNER JOIN (22.8 µs → 22.5 µs), LEFT JOIN (17.2 µs → 17.0 µs), 4-table JOIN (569.5 µs → 51.9 µs), Transaction ctl (47.0 µs → 37.6 µs).
- **Baseline kept by 4.41.1** (26): 4.42.0 is within 15% of it in 12
  categories and more than 15% slower in 14: INSERT single 3.80x, UPDATE point 3.57x, DELETE point 3.07x, TRUNCATE 1.41x, SET/SHOW/RESET 1.39x, REFRESH MATVIEW 1.39x, DISTINCT 1.38x, Aggregation 1.35x, CREATE TABLE 1.34x, Prepared stmts 1.30x, INSERT multi-row 1.22x, Scalar subquery 1.18x, UPDATE+subquery 1.17x, Point lookup 1.15x.

## Wire: one-row UPDATE by primary key against a running server

`extended` is a prepared statement bound per execution; `simple` is the statement sent as text.
Client and server share the host. Each client has its own connection and its own rows.

Median of 4 round(s); ratio is ops/s against `v4.41.1`.

| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |
|---|---|---:|---|---:|---:|---:|---:|---:|---|
| update | extended | 1 | v4.41.1 | 13,064 | 1.00 | 74 | 94 | 0 | 5.59/17.78/15.66/15.91 |
| update | extended | 1 | v4.42.0 | 9,693 | 0.74 | 102 | 126 | 0 | 16.57/10.42/15.37/17.08 |
| update | extended | 2 | v4.41.1 | 20,522 | 1.00 | 92 | 117 | 0 | 4.95/12.6/11.18/11.83 |
| update | extended | 2 | v4.42.0 | 11,967 | 0.58 | 164 | 216 | 0 | 11.8/7.74/11.04/11.79 |
| update | extended | 4 | v4.41.1 | 37,840 | 1.00 | 96 | 174 | 0 | 4.2/9.13/8.18/8.82 |
| update | extended | 4 | v4.42.0 | 24,366 | 0.64 | 154 | 271 | 0 | 9.36/6.37/8.63/8.55 |
| update | extended | 8 | v4.41.1 | 62,512 | 1.00 | 120 | 204 | 0 | 4.64/8.52/7.21/7.24 |
| update | extended | 8 | v4.42.0 | 41,992 | 0.67 | 181 | 316 | 0 | 7.87/6.17/7.61/7.45 |
| update | extended | 16 | v4.41.1 | 107,665 | 1.00 | 138 | 306 | 0 | 6.12/8.63/8.04/7.46 |
| update | extended | 16 | v4.42.0 | 67,816 | 0.63 | 222 | 574 | 0 | 8.33/7.81/7.93/7.96 |
| update | extended | 32 | v4.41.1 | 141,255 | 1.00 | 201 | 598 | 0 | 8.21/10.15/9.99/10.66 |
| update | extended | 32 | v4.42.0 | 69,942 | 0.50 | 395 | 1,128 | 0 | 10.94/10.29/8.69/9.58 |
| update | extended | 64 | v4.41.1 | 145,163 | 1.00 | 400 | 1,084 | 0 | 12.48/13.93/14.56/14.49 |
| update | extended | 64 | v4.42.0 | 69,009 | 0.48 | 818 | 5,943 | 0 | 14.9/13.96/12.97/13.03 |
| update | simple | 1 | v4.41.1 | 12,713 | 1.00 | 76 | 94 | 0 | 12.51/13.85/16.66/16.44 |
| update | simple | 1 | v4.42.0 | 7,756 | 0.61 | 123 | 194 | 0 | 16.08/16.89/14.78/13.6 |
| update | simple | 2 | v4.41.1 | 18,977 | 1.00 | 99 | 128 | 0 | 9.14/10.32/12.5/11.51 |
| update | simple | 2 | v4.42.0 | 10,120 | 0.53 | 194 | 257 | 0 | 11.17/13.09/10.71/10.78 |
| update | simple | 4 | v4.41.1 | 38,002 | 1.00 | 96 | 174 | 0 | 6.89/7.7/8.87/9.21 |
| update | simple | 4 | v4.42.0 | 21,933 | 0.58 | 170 | 302 | 0 | 8.14/9.35/7.75/8.17 |
| update | simple | 8 | v4.41.1 | 63,341 | 1.00 | 118 | 200 | 0 | 6.29/6.72/7.83/7.38 |
| update | simple | 8 | v4.42.0 | 40,400 | 0.64 | 188 | 340 | 0 | 7.43/8.61/7.41/7.65 |
| update | simple | 16 | v4.41.1 | 110,769 | 1.00 | 134 | 294 | 0 | 6.9/7.81/7.23/7.89 |
| update | simple | 16 | v4.42.0 | 65,903 | 0.59 | 227 | 596 | 0 | 8.11/9.78/9.29/8.38 |
| update | simple | 32 | v4.41.1 | 146,324 | 1.00 | 194 | 592 | 0 | 9.56/11.01/10.44/9.82 |
| update | simple | 32 | v4.42.0 | 72,790 | 0.50 | 388 | 1,154 | 0 | 11.47/12.69/11.97/11.94 |
| update | simple | 64 | v4.41.1 | 149,721 | 1.00 | 391 | 1,057 | 0 | 14.03/13.55/12.62/13.47 |
| update | simple | 64 | v4.42.0 | 67,959 | 0.45 | 840 | 5,726 | 0 | 12.16/15.06/14.93/14.85 |

How to read it: `load` is the host's 1-minute load when each round of a cell began; a 1-client cell
begins right after a 64-client cell, and the load average takes minutes to fall.

### With time travel off

The same 4.42.0 binary (sha256 `37cf8d142df6c4a1455d7709c3fa05eebbb985f67e8f69ec9b395d8f80ff42b6`), started with a configuration file
that holds only `[storage] time_travel_enabled = false`, against 4.41.1: 1, 16 and 64 clients, 2
rounds. A store that does not use `AS OF`, `VERSIONS BETWEEN` or branches from the past loses nothing
by it.

Median of 2 round(s); ratio is ops/s against `v4.41.1`.

| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |
|---|---|---:|---|---:|---:|---:|---:|---:|---|
| update | extended | 1 | v4.41.1 | 13,930 | 1.00 | 70 | 88 | 0 | 2.48/16.1 |
| update | extended | 1 | v4.42.0-tt-off | 12,360 | 0.89 | 80 | 99 | 0 | 13.46/17.08 |
| update | extended | 16 | v4.41.1 | 108,215 | 1.00 | 138 | 295 | 0 | 2.3/11.65 |
| update | extended | 16 | v4.42.0-tt-off | 100,117 | 0.93 | 152 | 296 | 0 | 9.69/12.09 |
| update | extended | 64 | v4.41.1 | 146,177 | 1.00 | 398 | 1,098 | 0 | 6.37/10.86 |
| update | extended | 64 | v4.42.0-tt-off | 132,689 | 0.91 | 437 | 1,182 | 0 | 11.91/12.75 |
| update | simple | 1 | v4.41.1 | 12,431 | 1.00 | 78 | 97 | 0 | 11.46/10.95 |
| update | simple | 1 | v4.42.0-tt-off | 11,703 | 0.94 | 86 | 104 | 0 | 15.91/15.18 |
| update | simple | 16 | v4.41.1 | 109,660 | 1.00 | 136 | 293 | 0 | 9.18/7.89 |
| update | simple | 16 | v4.42.0-tt-off | 102,752 | 0.94 | 148 | 300 | 0 | 12.1/11.05 |
| update | simple | 64 | v4.41.1 | 148,585 | 1.00 | 392 | 1,071 | 0 | 10.37/9.34 |
| update | simple | 64 | v4.42.0-tt-off | 138,192 | 0.93 | 422 | 1,116 | 0 | 13.75/12.25 |

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
| c=1 | 19,076 | 25,866 | 25,704 | 0.99 | 1.00, 0.99 |
| c=8 | 141,289 | 120,075 | 117,831 | 0.98 | 1.01, 0.95 |
| c=16 | 207,708 | 200,273 | 194,414 | 0.97 | 0.96, 0.99 |
| c=32 | 299,963 | 231,757 | 237,937 | 1.03 | 1.07, 0.99 |
| c=64 | 324,441 | 263,829 | 259,510 | 0.98 | 0.94, 1.03 |

**bulk-load COPY (rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 10000 | 76 | 49 | 55 | 1.12 | 1.21, 1.04 |
| 50000 | 78 | 104 | 108 | 1.04 | 1.11, 0.97 |
| 100000 | 109 | 170 | 180 | 1.05 | 1.06, 1.05 |

**indexed point-read (50k rows) [proto=simple]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 10,965 | 15,253 | 13,810 | 0.91 | 0.98, 0.84 |
| c=8 | 80,300 | 78,178 | 72,085 | 0.92 | 0.91, 0.93 |
| c=16 | 135,952 | 135,059 | 128,586 | 0.95 | 0.95, 0.95 |
| c=32 | 181,325 | 178,956 | 168,276 | 0.94 | 0.91, 0.97 |
| c=64 | 228,744 | 177,026 | 169,190 | 0.96 | 0.90, 1.01 |

**indexed point-read (50k rows) [proto=extended]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 9,812 | 9,691 | 8,471 | 0.87 | 0.91, 0.84 |
| c=8 | 74,835 | 50,620 | 46,119 | 0.91 | 0.90, 0.92 |
| c=16 | 124,501 | 93,083 | 84,287 | 0.91 | 0.92, 0.89 |
| c=32 | 169,350 | 119,973 | 105,558 | 0.88 | 0.87, 0.89 |
| c=64 | 207,907 | 121,252 | 107,674 | 0.89 | 0.91, 0.87 |

**indexed point-read (50k rows) [proto=prepared]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 17,611 | 16,567 | 12,737 | 0.77 | 0.76, 0.78 |
| c=8 | 131,912 | 83,510 | 70,403 | 0.84 | 0.84, 0.85 |
| c=16 | 192,224 | 150,266 | 124,795 | 0.83 | 0.82, 0.84 |
| c=32 | 280,861 | 195,614 | 170,002 | 0.87 | 0.88, 0.86 |
| c=64 | 305,923 | 208,511 | 169,766 | 0.81 | 0.81, 0.82 |

**DROP TABLE (100k rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 100k | 60 | 124 | 148 | 1.19 | 1.26, 1.12 |

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
  it are listed in the changelog (*a second pass over what 4.42.0 added to a statement* and *a third
  pass: joins under a filter*). What those
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
