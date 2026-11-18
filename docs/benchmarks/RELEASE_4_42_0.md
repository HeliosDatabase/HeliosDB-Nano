# HeliosDB-Nano 4.42.0 against 4.41.1: the release benchmark

The 4.42.0 release build measured against the 4.41.1 release build, on one host, on 2026-11-18, with
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
  slower in 14 (INSERT single 3.85x, UPDATE point 3.44x, DELETE point 3.21x, DROP TABLE 1.49x, TRUNCATE 1.49x, CREATE TABLE 1.47x, Prepared stmts 1.47x, REFRESH MATVIEW 1.43x, SET/SHOW/RESET 1.35x, INSERT multi-row 1.26x, UPDATE+subquery 1.25x, Point lookup 1.21x, DISTINCT 1.19x, IN subquery 1.17x); faster in 6 (4-table JOIN 0.09x, INSERT..SELECT 0.12x, ALTER TABLE 0.29x, Transaction ctl 0.74x, INNER JOIN 0.82x, LEFT JOIN 0.85x).
- **What the performance passes changed** (`v4.42.0` against `v4.42.0-2a5d741a`, same run): faster by 13% or more in
  15 categories (4-table JOIN 0.07x, ALTER TABLE 0.40x, Transaction ctl 0.58x, DROP TABLE 0.62x, UPSERT 0.68x, CREATE/DROP VIEW 0.69x, CREATE TABLE 0.70x, CREATE INDEX 0.70x, UPDATE+subquery 0.77x, LEFT JOIN 0.78x, INNER JOIN 0.80x, TRUNCATE 0.82x, INSERT multi-row 0.83x, SET/SHOW/RESET 0.85x, INSERT..SELECT 0.87x); slower by 15% or more in 1 (IN subquery 1.22x).
  The rest are within the noise of two rounds.
- **One-row `UPDATE` over the wire** runs at 0.45–0.74 of 4.41.1 prepared
  (lowest at 64 clients, highest at 1) and
  0.46–0.65 as text (lowest at 64, highest at
  8). Each such commit records the row's history and its snapshot:
  seven keys where 4.41.1 wrote one. See *What is slower, and why*.
- **Against the baseline** (the best time each category has reached in a release, best round of each):
  4.42.0 sets a new baseline in 9 of the 35 categories (ALTER TABLE, INSERT..SELECT, UPSERT, INNER JOIN, LEFT JOIN, 4-table JOIN, Window funcs, LIKE/BETWEEN/IN, Transaction ctl).
  In the other 26 the baseline stays with 4.41.1: 4.42.0 is within 15% of it in
  13 and more than 15% slower in 13 (INSERT single 3.81x, UPDATE point 3.76x, DELETE point 2.94x, CREATE TABLE 1.58x, DROP TABLE 1.49x, SET/SHOW/RESET 1.40x, TRUNCATE 1.40x, REFRESH MATVIEW 1.36x, Prepared stmts 1.28x, IN subquery 1.27x, INSERT multi-row 1.25x, DISTINCT 1.22x, UPDATE+subquery 1.19x).
- **With `[storage] time_travel_enabled = false`** the same server writes what 4.41.1 wrote and
  runs the one-row `UPDATE` at 0.90–0.96 of 4.41.1 (1, 16 and 64 clients, both protocols).
- **pgbench**: the indexed point read runs at 0.92–0.98 of 4.41.1 as
  text, 0.88–0.91 in the extended protocol and
  0.83–0.90 prepared; `SELECT 1`, `COPY` and `DROP TABLE` are in the
  tables below.

## What was measured

| | |
|---|---|
| date (UTC) | 2026-11-18T06:15:19Z |
| platform | Linux x86_64, 32 logical CPUs, at least 64 GiB of memory |
| toolchain | rustc 1.100.0 (0ef1bc386 2026-11-09); cargo 1.100.0 (3acc79c0c 2026-11-05) |
| v4.42.0 | commit `1e20624ba0883420b23fc065bb71a209d9d28cd3`, 2 uncommitted file(s) |
| v4.41.1 | commit `4af19e1c447b8b9f8a621a984e8cc0558195bf3b`, pg35 harness files copied from v4.42.0 |
| `bin/v4.42.0/heliosdb-nano` | sha256 `86026c2a8f36fec4587a6164baa54327136db1b6629be5408c7ca331d34a156c` |
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
| CREATE TABLE | 30.30 ms | 162.8 us | 186.1 | 345.1 us | 87.8 | 240.0 us | 126.3 | 2.12 | 1.47 |
| CREATE INDEX | 29.03 ms | 292.6 us | 99.2 | 480.4 us | 60.4 | 335.4 us | 86.6 | 1.64 | 1.15 |
| ALTER TABLE | 27.50 ms | 930.3 us | 29.6 | 666.8 us | 41.2 | 267.5 us | 102.8 | 0.72 | 0.29 |
| DROP TABLE | 13.12 ms | 147.9 us | 88.7 | 356.3 us | 36.8 | 220.9 us | 59.4 | 2.41 | 1.49 |
| CREATE/DROP VIEW | 26.60 ms | 120.5 us | 220.8 | 172.7 us | 154.0 | 119.3 us | 223.0 | 1.43 | 0.99 |
| REFRESH MATVIEW | 17.46 ms | 589.8 us | 29.6 | 791.7 us | 22.1 | 840.9 us | 20.8 | 1.34 | 1.43 |
| TRUNCATE | 82.08 ms | 154.4 us | 531.7 | 280.7 us | 292.4 | 229.4 us | 357.7 | 1.82 | 1.49 |
| INSERT single | 13.36 ms | 6.65 us | 2,008.5 | 25.63 us | 521.0 | 25.61 us | 521.5 | 3.85 | 3.85 |
| INSERT multi-row | 12.77 ms | 150.8 us | 84.7 | 228.3 us | 55.9 | 189.9 us | 67.2 | 1.51 | 1.26 |
| INSERT..SELECT | 13.60 ms | 1.45 ms | 9.4 | 203.7 us | 66.8 | 176.4 us | 77.1 | 0.14 | 0.12 |
| UPDATE point | 13.12 ms | 10.60 us | 1,238.2 | 38.45 us | 341.4 | 36.49 us | 359.7 | 3.63 | 3.44 |
| DELETE point | 13.73 ms | 4.01 us | 3,419.3 | 11.94 us | 1,150.1 | 12.88 us | 1,066.0 | 2.97 | 3.21 |
| UPSERT | 13.72 ms | 61.50 us | 223.1 | 97.39 us | 140.9 | 66.64 us | 205.9 | 1.58 | 1.08 |
| UPDATE+subquery | 13.50 ms | 124.1 us | 108.8 | 200.2 us | 67.4 | 154.6 us | 87.3 | 1.61 | 1.25 |
| Point lookup | 199.0 us | 2.78 us | 71.5 | 3.26 us | 61.0 | 3.37 us | 59.0 | 1.17 | 1.21 |
| Full scan+filter | 265.7 us | 27.83 us | 9.5 | 28.18 us | 9.4 | 27.45 us | 9.7 | 1.01 | 0.99 |
| Aggregation | 270.7 us | 1.17 us | 230.5 | 1.33 us | 203.9 | 1.18 us | 229.8 | 1.13 | 1.00 |
| INNER JOIN | 246.9 us | 28.86 us | 8.6 | 29.62 us | 8.3 | 23.57 us | 10.5 | 1.03 | 0.82 |
| LEFT JOIN | 260.0 us | 20.59 us | 12.6 | 22.64 us | 11.5 | 17.55 us | 14.8 | 1.10 | 0.85 |
| 4-table JOIN | 521.7 us | 641.1 us | 0.8 | 858.2 us | 0.6 | 58.66 us | 8.9 | 1.34 | 0.09 |
| Scalar subquery | 348.1 us | 2.65 us | 131.5 | 3.03 us | 115.0 | 2.72 us | 127.8 | 1.14 | 1.03 |
| EXISTS subquery | 463.1 us | 5.98 us | 77.4 | 4.81 us | 96.2 | 5.30 us | 87.4 | 0.80 | 0.89 |
| IN subquery | 398.0 us | 5.23 us | 76.1 | 4.99 us | 79.7 | 6.11 us | 65.2 | 0.95 | 1.17 |
| CTE | 465.8 us | 7.13 us | 65.3 | 6.36 us | 73.2 | 6.52 us | 71.4 | 0.89 | 0.91 |
| Recursive CTE | 311.8 us | 3.13 us | 99.8 | 3.61 us | 86.4 | 3.22 us | 97.0 | 1.16 | 1.03 |
| Window funcs | 504.6 us | 20.70 us | 24.4 | 20.01 us | 25.2 | 19.49 us | 25.9 | 0.97 | 0.94 |
| UNION | 311.6 us | 8.32 us | 37.5 | 8.71 us | 35.8 | 8.61 us | 36.2 | 1.05 | 1.03 |
| DISTINCT | 211.3 us | 0.72 us | 292.5 | 0.86 us | 246.6 | 0.86 us | 246.3 | 1.19 | 1.19 |
| ORDER+LIMIT | 276.0 us | 98.18 us | 2.8 | 95.73 us | 2.9 | 94.16 us | 2.9 | 0.98 | 0.96 |
| CASE expr | 248.4 us | 18.06 us | 13.8 | 16.22 us | 15.3 | 16.22 us | 15.3 | 0.90 | 0.90 |
| LIKE/BETWEEN/IN | 264.2 us | 12.46 us | 21.2 | 11.12 us | 23.7 | 12.12 us | 21.8 | 0.89 | 0.97 |
| String ops | 255.4 us | 14.40 us | 17.7 | 12.87 us | 19.8 | 13.97 us | 18.3 | 0.89 | 0.97 |
| Transaction ctl | 13.71 ms | 50.52 us | 271.5 | 64.17 us | 213.7 | 37.40 us | 366.7 | 1.27 | 0.74 |
| Prepared stmts | 452.7 us | 3.22 us | 140.8 | 4.66 us | 97.2 | 4.74 us | 95.6 | 1.45 | 1.47 |
| SET/SHOW/RESET | 352.3 us | 1.98 us | 177.7 | 3.16 us | 111.6 | 2.67 us | 131.9 | 1.59 | 1.35 |

- `v4.41.1`: faster than PostgreSQL in 34 of 35 categories.
- `v4.42.0-2a5d741a`: faster than PostgreSQL in 34 of 35 categories.
- `v4.42.0`: faster than PostgreSQL in 35 of 35 categories.

| run | load before | load after |
|---|---:|---:|
| v4_41_1-r1 | 2.44 | 2.91 |
| v4_42_0-2a5d741a-r1 | 2.91 | 2.56 |
| v4_42_0-r1 | 2.56 | 2.43 |
| v4_42_0-r2 | 2.43 | 2.41 |
| v4_42_0-2a5d741a-r2 | 2.41 | 2.75 |
| v4_41_1-r2 | 2.75 | 2.1 |

The harness checks every statement's result against an independent model on both engines; a run
with a wrong result produces no number. A cell is the mean of 300 timed statements; the table shows
the median of the rounds, and `summary.json` has every round.

Two rounds do not settle a 15% line everywhere. Pairing each round of 4.42.0 with each round of
4.41.1, 11 of the 35 categories fall on different sides of it depending on the pair
(CREATE INDEX 1.01–1.30, INNER JOIN 0.73–0.91, LEFT JOIN 0.77–0.94, Scalar subquery 0.87–1.22, EXISTS subquery 0.81–0.96, IN subquery 1.08–1.27, CTE 0.78–1.06, Recursive CTE 0.91–1.17, UNION 0.82–1.32, DISTINCT 1.06–1.34, CASE expr 0.86–0.93); the other 24 are in the same class for
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
| CREATE TABLE | 150 µs | 4.41.1 | 150 µs | 237 µs |
| CREATE INDEX | 270 µs | 4.41.1 | 270 µs | 308 µs |
| ALTER TABLE | 252 µs | 4.42.0 | 863 µs | 252 µs |
| DROP TABLE | 141 µs | 4.41.1 | 141 µs | 210 µs |
| CREATE/DROP VIEW | 117 µs | 4.41.1 | 117 µs | 117 µs |
| REFRESH MATVIEW | 531 µs | 4.41.1 | 531 µs | 721 µs |
| TRUNCATE | 153 µs | 4.41.1 | 153 µs | 213 µs |
| INSERT single | 6.54 µs | 4.41.1 | 6.54 µs | 24.9 µs |
| INSERT multi-row | 150 µs | 4.41.1 | 150 µs | 187 µs |
| INSERT..SELECT | 162 µs | 4.42.0 | 1.27 ms | 162 µs |
| UPDATE point | 9.18 µs | 4.41.1 | 9.18 µs | 34.5 µs |
| DELETE point | 3.93 µs | 4.41.1 | 3.93 µs | 11.6 µs |
| UPSERT | 60.1 µs | 4.42.0 | 61.0 µs | 60.1 µs |
| UPDATE+subquery | 124 µs | 4.41.1 | 124 µs | 147 µs |
| Point lookup | 2.66 µs | 4.41.1 | 2.66 µs | 2.93 µs |
| Full scan+filter | 26.0 µs | 4.41.1 | 26.0 µs | 26.7 µs |
| Aggregation | 0.99 µs | 4.41.1 | 0.99 µs | 1.11 µs |
| INNER JOIN | 22.0 µs | 4.42.0 | 22.8 µs | 22.0 µs |
| LEFT JOIN | 16.7 µs | 4.42.0 | 17.2 µs | 16.7 µs |
| 4-table JOIN | 56.1 µs | 4.42.0 | 570 µs | 56.1 µs |
| Scalar subquery | 2.43 µs | 4.41.1 | 2.43 µs | 2.46 µs |
| EXISTS subquery | 4.50 µs | 4.41.1 | 4.50 µs | 4.70 µs |
| IN subquery | 4.54 µs | 4.41.1 | 4.54 µs | 5.77 µs |
| CTE | 5.52 µs | 4.41.1 | 5.52 µs | 5.61 µs |
| Recursive CTE | 2.86 µs | 4.41.1 | 2.86 µs | 2.88 µs |
| Window funcs | 18.4 µs | 4.42.0 | 19.2 µs | 18.4 µs |
| UNION | 7.08 µs | 4.41.1 | 7.08 µs | 7.44 µs |
| DISTINCT | 0.64 µs | 4.41.1 | 0.64 µs | 0.78 µs |
| ORDER+LIMIT | 83.8 µs | 4.41.1 | 83.8 µs | 86.1 µs |
| CASE expr | 14.8 µs | 4.41.1 | 14.8 µs | 15.2 µs |
| LIKE/BETWEEN/IN | 9.82 µs | 4.42.0 | 9.99 µs | 9.82 µs |
| String ops | 11.8 µs | 4.41.1 | 11.8 µs | 12.2 µs |
| Transaction ctl | 35.4 µs | 4.42.0 | 47.0 µs | 35.4 µs |
| Prepared stmts | 3.20 µs | 4.41.1 | 3.20 µs | 4.10 µs |
| SET/SHOW/RESET | 1.81 µs | 4.41.1 | 1.81 µs | 2.54 µs |

- **New baselines set by 4.42.0** (9): ALTER TABLE (863.3 µs → 252.4 µs), INSERT..SELECT (1.27 ms → 161.7 µs), UPSERT (61.0 µs → 60.1 µs), INNER JOIN (22.8 µs → 22.0 µs), LEFT JOIN (17.2 µs → 16.7 µs), 4-table JOIN (569.5 µs → 56.1 µs), Window funcs (19.2 µs → 18.4 µs), LIKE/BETWEEN/IN (9.99 µs → 9.82 µs), Transaction ctl (47.0 µs → 35.4 µs).
- **Baseline kept by 4.41.1** (26): 4.42.0 is within 15% of it in 13
  categories and more than 15% slower in 13: INSERT single 3.81x, UPDATE point 3.76x, DELETE point 2.94x, CREATE TABLE 1.58x, DROP TABLE 1.49x, SET/SHOW/RESET 1.40x, TRUNCATE 1.40x, REFRESH MATVIEW 1.36x, Prepared stmts 1.28x, IN subquery 1.27x, INSERT multi-row 1.25x, DISTINCT 1.22x, UPDATE+subquery 1.19x.

## Wire: one-row UPDATE by primary key against a running server

`extended` is a prepared statement bound per execution; `simple` is the statement sent as text.
Client and server share the host. Each client has its own connection and its own rows.

Median of 4 round(s); ratio is ops/s against `v4.41.1`.

| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |
|---|---|---:|---|---:|---:|---:|---:|---:|---|
| update | extended | 1 | v4.41.1 | 13,134 | 1.00 | 74 | 95 | 0 | 2.1/14.23/15.29/16.49 |
| update | extended | 1 | v4.42.0 | 9,771 | 0.74 | 100 | 128 | 0 | 16.95/9.19/14.51/15.28 |
| update | extended | 2 | v4.41.1 | 20,610 | 1.00 | 92 | 117 | 0 | 2.29/10.95/10.76/12.26 |
| update | extended | 2 | v4.42.0 | 12,147 | 0.59 | 162 | 218 | 0 | 12.05/6.69/11.03/11.57 |
| update | extended | 4 | v4.41.1 | 37,447 | 1.00 | 98 | 177 | 0 | 2.13/7.9/7.84/8.87 |
| update | extended | 4 | v4.42.0 | 24,424 | 0.65 | 152 | 271 | 0 | 9.36/5.07/7.9/8.4 |
| update | extended | 8 | v4.41.1 | 62,380 | 1.00 | 120 | 204 | 0 | 2.72/7.17/7.01/7.42 |
| update | extended | 8 | v4.42.0 | 42,144 | 0.68 | 180 | 324 | 0 | 7.58/4.98/7.22/7.08 |
| update | extended | 16 | v4.41.1 | 107,356 | 1.00 | 138 | 305 | 0 | 5.13/7.36/7.28/7.73 |
| update | extended | 16 | v4.42.0 | 66,243 | 0.62 | 226 | 608 | 0 | 8.37/6.7/7.5/7.92 |
| update | extended | 32 | v4.41.1 | 143,097 | 1.00 | 198 | 616 | 0 | 8.31/9.95/9.52/10.51 |
| update | extended | 32 | v4.42.0 | 67,907 | 0.47 | 398 | 1,098 | 0 | 11.47/9.93/9.42/11.4 |
| update | extended | 64 | v4.41.1 | 145,760 | 1.00 | 400 | 1,090 | 0 | 11.66/12.35/13.63/12.63 |
| update | extended | 64 | v4.42.0 | 66,113 | 0.45 | 826 | 6,246 | 0 | 14.57/13.97/13.64/13.32 |
| update | simple | 1 | v4.41.1 | 12,427 | 1.00 | 77 | 97 | 0 | 11.12/12.52/16.14/15.63 |
| update | simple | 1 | v4.42.0 | 7,657 | 0.62 | 123 | 197 | 0 | 16.53/16.3/16.76/15.03 |
| update | simple | 2 | v4.41.1 | 18,942 | 1.00 | 100 | 126 | 0 | 7.87/9.51/12.53/10.97 |
| update | simple | 2 | v4.42.0 | 10,217 | 0.54 | 192 | 260 | 0 | 11.61/12.67/12.68/10.8 |
| update | simple | 4 | v4.41.1 | 37,750 | 1.00 | 96 | 176 | 0 | 6.0/7.29/9.02/8.48 |
| update | simple | 4 | v4.42.0 | 21,982 | 0.58 | 170 | 304 | 0 | 8.85/9.01/10.41/8.43 |
| update | simple | 8 | v4.41.1 | 63,338 | 1.00 | 118 | 201 | 0 | 5.43/6.27/7.7/7.3 |
| update | simple | 8 | v4.42.0 | 41,375 | 0.65 | 184 | 340 | 0 | 8.1/8.17/8.94/7.49 |
| update | simple | 16 | v4.41.1 | 110,308 | 1.00 | 134 | 305 | 0 | 5.89/7.13/8.14/7.78 |
| update | simple | 16 | v4.42.0 | 65,574 | 0.59 | 228 | 612 | 0 | 9.2/9.29/9.13/8.14 |
| update | simple | 32 | v4.41.1 | 144,036 | 1.00 | 197 | 622 | 0 | 8.78/10.23/10.47/10.1 |
| update | simple | 32 | v4.42.0 | 72,323 | 0.50 | 390 | 1,050 | 0 | 11.4/12.06/11.33/11.84 |
| update | simple | 64 | v4.41.1 | 147,960 | 1.00 | 394 | 1,078 | 0 | 13.32/12.12/13.11/13.04 |
| update | simple | 64 | v4.42.0 | 67,834 | 0.46 | 842 | 5,676 | 0 | 9.78/14.71/13.85/14.86 |

How to read it: `load` is the host's 1-minute load when each round of a cell began; a 1-client cell
begins right after a 64-client cell, and the load average takes minutes to fall.

### With time travel off

The same 4.42.0 binary (sha256 `86026c2a8f36fec4587a6164baa54327136db1b6629be5408c7ca331d34a156c`), started with a configuration file
that holds only `[storage] time_travel_enabled = false`, against 4.41.1: 1, 16 and 64 clients, 2
rounds. A store that does not use `AS OF`, `VERSIONS BETWEEN` or branches from the past loses nothing
by it.

Median of 2 round(s); ratio is ops/s against `v4.41.1`.

| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |
|---|---|---:|---|---:|---:|---:|---:|---:|---|
| update | extended | 1 | v4.41.1 | 13,590 | 1.00 | 72 | 90 | 0 | 4.02/14.53 |
| update | extended | 1 | v4.42.0-tt-off | 12,280 | 0.90 | 82 | 99 | 0 | 13.26/15.83 |
| update | extended | 16 | v4.41.1 | 107,310 | 1.00 | 138 | 317 | 0 | 3.26/10.31 |
| update | extended | 16 | v4.42.0-tt-off | 102,924 | 0.96 | 148 | 314 | 0 | 9.44/11.06 |
| update | extended | 64 | v4.41.1 | 142,089 | 1.00 | 411 | 1,087 | 0 | 6.89/12.04 |
| update | extended | 64 | v4.42.0-tt-off | 134,791 | 0.95 | 433 | 1,150 | 0 | 11.09/12.89 |
| update | simple | 1 | v4.41.1 | 12,576 | 1.00 | 76 | 96 | 0 | 10.32/15.12 |
| update | simple | 1 | v4.42.0-tt-off | 11,469 | 0.91 | 86 | 106 | 0 | 14.86/14.29 |
| update | simple | 16 | v4.41.1 | 109,309 | 1.00 | 136 | 290 | 0 | 7.68/10.75 |
| update | simple | 16 | v4.42.0-tt-off | 104,874 | 0.96 | 146 | 276 | 0 | 10.57/10.45 |
| update | simple | 64 | v4.41.1 | 147,716 | 1.00 | 395 | 1,083 | 0 | 9.53/11.15 |
| update | simple | 64 | v4.42.0-tt-off | 139,112 | 0.94 | 422 | 1,096 | 0 | 11.78/11.67 |

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
| c=1 | 19,081 | 26,329 | 25,860 | 0.98 | 1.02, 0.94 |
| c=8 | 140,185 | 120,087 | 120,648 | 1.00 | 0.98, 1.03 |
| c=16 | 212,927 | 198,130 | 195,689 | 0.99 | 0.99, 0.98 |
| c=32 | 294,678 | 245,080 | 242,632 | 0.99 | 0.96, 1.02 |
| c=64 | 326,626 | 245,296 | 266,112 | 1.08 | 1.08, 1.09 |

**bulk-load COPY (rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 10000 | 71 | 50 | 53 | 1.05 | 1.06, 1.04 |
| 50000 | 82 | 100 | 106 | 1.06 | 1.09, 1.04 |
| 100000 | 93 | 160 | 178 | 1.11 | 1.14, 1.08 |

**indexed point-read (50k rows) [proto=simple]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 10,966 | 13,870 | 13,168 | 0.95 | 0.97, 0.93 |
| c=8 | 80,463 | 76,704 | 74,268 | 0.97 | 0.96, 0.98 |
| c=16 | 133,633 | 139,017 | 129,063 | 0.93 | 0.92, 0.94 |
| c=32 | 187,847 | 175,277 | 172,247 | 0.98 | 1.01, 0.95 |
| c=64 | 221,765 | 179,128 | 165,332 | 0.92 | 0.92, 0.93 |

**indexed point-read (50k rows) [proto=extended]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 10,073 | 9,906 | 8,700 | 0.88 | 0.94, 0.82 |
| c=8 | 74,127 | 51,815 | 46,495 | 0.90 | 0.89, 0.91 |
| c=16 | 126,629 | 93,384 | 84,750 | 0.91 | 0.92, 0.90 |
| c=32 | 169,069 | 120,143 | 105,583 | 0.88 | 0.89, 0.87 |
| c=64 | 205,419 | 121,347 | 107,907 | 0.89 | 0.90, 0.88 |

**indexed point-read (50k rows) [proto=prepared]** (tps, higher is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| c=1 | 17,904 | 15,013 | 13,440 | 0.90 | 0.97, 0.82 |
| c=8 | 129,179 | 83,924 | 70,014 | 0.83 | 0.83, 0.84 |
| c=16 | 194,860 | 143,766 | 125,312 | 0.87 | 0.89, 0.86 |
| c=32 | 273,708 | 201,949 | 166,937 | 0.83 | 0.82, 0.83 |
| c=64 | 313,855 | 202,377 | 171,245 | 0.85 | 0.85, 0.84 |

**DROP TABLE (100k rows -> ms)** (ms, lower is better)

| cell | PostgreSQL | Nano 4.41.1 | Nano 4.42.0 | 4.42.0 / 4.41.1 | per run |
|---|---:|---:|---:|---:|---|
| 100k | 52 | 132 | 142 | 1.07 | 1.04, 1.11 |

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
