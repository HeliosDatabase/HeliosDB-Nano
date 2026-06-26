# HeliosDB-Nano vs PostgreSQL 18.4 — the `pg35` benchmark

`pg35` is HeliosDB-Nano's primary, head-to-head performance benchmark: **35
SQL categories** run identically against an embedded HeliosDB-Nano instance and
a real **PostgreSQL 18.4** server, on the same machine, same data, same queries.
It is the *benchmark of record* — every engine change is gated against it so the
published numbers stay honest and erosion-tracked.

The harness is committed: [`tests/pg35_benchmark.rs`](../../tests/pg35_benchmark.rs);
the per-category history lives in
[`perf/v358_program/pg35_category_history.json`](../../perf/v358_program/pg35_category_history.json).

> **Read this honestly.** The headline is *not* "35–0". Nano wins **30 of 35
> categories by enormous margins** (10×–35,000×). The remaining **5 — the join,
> top-k, and prepared-statement categories — are near parity** (within ~2× either
> way), and on a less-noisy run PostgreSQL edges **2 of them**. Those 5 are the
> active performance-engineering frontier; the blow-out 30 are not close.

## Latest run — v3.60.1 (2026-06-26)

100 iterations/category, against PostgreSQL 18.4. **Scoreboard: Nano 33 · PG 2 ·
ties 0.** (See *Methodology* for why the boundary categories are noise-sensitive
and why this is not yet a *certified* quiet-host number.)

| Category | HeliosDB-Nano | PostgreSQL 18.4 | Speedup | Winner |
|---|---:|---:|---:|:--:|
| CREATE TABLE | 120us | 26.20ms | 218.68× faster | 🟢 Nano |
| CREATE INDEX | 347us | 25.56ms | 73.73× faster | 🟢 Nano |
| ALTER TABLE | 727us | 22.99ms | 31.63× faster | 🟢 Nano |
| DROP TABLE | 67.0us | 12.02ms | 179.21× faster | 🟢 Nano |
| CREATE/DROP VIEW | 207us | 22.56ms | 108.80× faster | 🟢 Nano |
| REFRESH MATVIEW | 370us | 11.72ms | 31.65× faster | 🟢 Nano |
| TRUNCATE | 126us | 69.71ms | 555.27× faster | 🟢 Nano |
| INSERT single | 7.61us | 10.88ms | 1430.43× faster | 🟢 Nano |
| INSERT multi-row | 229us | 10.15ms | 44.33× faster | 🟢 Nano |
| INSERT..SELECT | 527us | 12.07ms | 22.88× faster | 🟢 Nano |
| UPDATE point | 73.9us | 12.11ms | 163.86× faster | 🟢 Nano |
| DELETE point | 3.97us | 11.20ms | 2822.11× faster | 🟢 Nano |
| UPSERT | 59.2us | 11.15ms | 188.26× faster | 🟢 Nano |
| UPDATE+subquery | 169us | 13.31ms | 78.65× faster | 🟢 Nano |
| Point lookup | 3.25us | 309us | 95.08× faster | 🟢 Nano |
| Full scan+filter | 26.3us | 351us | 13.34× faster | 🟢 Nano |
| Aggregation | 0.65us | 379us | 587.47× faster | 🟢 Nano |
| INNER JOIN | 309us | 354us | 1.15× faster | 🟢 Nano |
| LEFT JOIN | 255us | 358us | 1.40× faster | 🟢 Nano |
| 4-table JOIN | 661us | 609us | 1.08× slower | 🔴 PG |
| Scalar subquery | 2.00us | 445us | 222.17× faster | 🟢 Nano |
| EXISTS subquery | 0.28us | 580us | 2101.12× faster | 🟢 Nano |
| IN subquery | 5.84us | 501us | 85.84× faster | 🟢 Nano |
| CTE | 21.3us | 641us | 30.07× faster | 🟢 Nano |
| Recursive CTE | 2.11us | 418us | 198.62× faster | 🟢 Nano |
| Window funcs | 20.4us | 566us | 27.83× faster | 🟢 Nano |
| UNION | 6.92us | 416us | 60.15× faster | 🟢 Nano |
| DISTINCT | 0.49us | 324us | 655.38× faster | 🟢 Nano |
| ORDER+LIMIT | 231us | 426us | 1.85× faster | 🟢 Nano |
| CASE expr | 18.7us | 363us | 19.41× faster | 🟢 Nano |
| LIKE/BETWEEN/IN | 13.7us | 465us | 33.99× faster | 🟢 Nano |
| String ops | 16.2us | 495us | 30.56× faster | 🟢 Nano |
| Transaction ctl | 0.38us | 13.62ms | 35735.75× faster | 🟢 Nano |
| Prepared stmts | 1.38ms | 711us | 1.94× slower | 🔴 PG |
| SET/SHOW/RESET | 7.53us | 599us | 79.59× faster | 🟢 Nano |

### Where PostgreSQL leads (the frontier)

- **Prepared stmts (≈1.9× slower this run).** The extended-protocol
  Parse/Bind/Execute path is the one category PostgreSQL has historically edged
  (it sat at ~1.0× for most of the v3.58 line). It is also the **highest-variance**
  category on a shared host. The known lever is reducing per-`Parse` allocation /
  re-planning; it is a tracked, deferred optimization.
- **4-table JOIN (≈1.08× slower this run).** This sits right on the tie line —
  it has oscillated between ~0.81× and ~1.08× across the history below. The
  index-nested-loop join already streams its inner fetch (v3.60.0), but join
  ordering / build-side selection on the widest shape is the remaining lever.

The two-way joins (**INNER 1.15×, LEFT 1.40×**) and **ORDER+LIMIT (1.85×)** are
Nano wins here.

## Evolution — how the boundary categories have moved

Per-category ratio = `nano_time / pg_time` (**< 1.0 = Nano faster**). The 30
blow-out categories are omitted (they are not close and don't move); these 5 are
the ones that decide the scoreboard.

| Snapshot | INNER JOIN | LEFT JOIN | 4-table JOIN | ORDER+LIMIT | Prepared stmts |
|---|---:|---:|---:|---:|---:|
| items1+4+8+lz4embedfix | 0.800 | 0.716 | 0.873 | 0.853 | 0.885 |
| item2c_copy_from_stdin | 0.822 | 0.760 | 0.954 | 1.027 | 0.938 |
| item1b_writepath_plancache_fix | 0.766 | 0.708 | 0.869 | 0.973 | 1.121 |
| candidate_c_volatile_counter | 0.732 | 0.680 | 0.815 | 0.684 | 1.066 |
| v3.58-13item-fixes-2026-06-18 | 0.852 | 0.767 | 1.022 | 0.877 | 1.054 |
| v3.58-13item-fixes-rerun-2026-06-18 | 0.854 | 0.796 | 0.977 | 1.036 | 1.023 |
| **v3.60.1 (100-iter, shared host)** | **0.870** | **0.714** | **1.080** | **0.541** | **1.940** |

Reading it: **INNER/LEFT JOIN are consistently Nano-favoured** (and v3.60.0's
index-nested-loop `Arc` inner-fetch helps). **4-table JOIN and Prepared stmts
straddle 1.0×** — the categories that flip the scoreboard between 33/35 and 35/35
depending on the run and host load.

## Methodology & honesty notes

- **Identical workload.** The same 35 statement classes (DDL, DML, point ops,
  joins, subqueries, CTEs, window functions, aggregates, the extended protocol)
  run against embedded Nano and a real PostgreSQL 18.4 over `tokio-postgres`,
  same schema and row counts, timed the same way.
- **How to reproduce.** Start PostgreSQL 18.4 (e.g. a `postgres:18` container on
  `:25433`, user `bench`/`benchpass`, db `benchdb`) and run:

  ```
  PG35_ITERS=100 cargo test --release --test pg35_benchmark -- --ignored --nocapture
  ```

  Override the DSN with `PG35_CONNSTR` and the label with `PG35_PG_LABEL`.
- **Why "33–2" and not "35–0".** A low-iteration run on this shared development
  host showed 35–0, but that was measurement noise: the 5 boundary categories
  are near parity, so a handful of iterations easily flips them. The 100-iteration
  run above is more reliable and shows **33–2**. None of this changes the
  blow-out 30.
- **Not yet certified.** This box runs other workloads concurrently, which
  inflates variance on the near-parity categories (Prepared stmts especially).
  The *certified* number must come from an **idle, dedicated host**; treat the
  boundary categories here as directional, not final.
- **No cherry-picking.** All 35 categories are reported, including the 2 losses.
  The harness and the full history JSON are in the repo for independent runs.
