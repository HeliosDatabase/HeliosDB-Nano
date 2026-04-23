#!/usr/bin/env python3
"""
Pagination Benchmark — HeliosDB Nano vs any PG-wire-compatible RDBMS
====================================================================

Measures p50/p95/p99 latency for the three pagination shapes the
`FEATURE_REQUEST_pagination.md` acceptance criteria care about:

  1. Offset pagination:  SELECT ... ORDER BY id LIMIT N OFFSET M
  2. Keyset pagination:  SELECT ... WHERE id < $last ORDER BY id DESC LIMIT N
  3. Join pagination:    SELECT ... LEFT JOIN ... ORDER BY id LIMIT N OFFSET M

The script is PG-wire-native (psycopg), so it runs unchanged against
PostgreSQL 13+/14/15/16, HeliosDB Nano, CockroachDB, YugabyteDB, or any
other wire-compatible backend. For MySQL, use `pagination_bench_mysql.py`.

For Oracle / MS SQL Server, reproduce the same queries through their
native CLI and paste the p50/p95/p99 into the marketing page — we don't
bundle proprietary drivers here.

Usage
-----
  # HeliosDB Nano (Unix socket)
  python3 pagination_bench.py --host /tmp --port 5432 --name "Nano"

  # PostgreSQL
  PGPASSWORD=postgres python3 pagination_bench.py \\
      --host localhost --port 5432 --user postgres --name "PostgreSQL 16"

  # Compare results
  python3 pagination_bench.py --compare nano.json pg16.json
"""

from __future__ import annotations
import argparse
import json
import statistics
import sys
import time
from pathlib import Path

try:
    import psycopg
except ImportError:
    print("ERROR: pip install 'psycopg[binary]'", file=sys.stderr)
    sys.exit(1)


# ─────────────────────────────────────────────────────────────────────────────
# Harness
# ─────────────────────────────────────────────────────────────────────────────

def bench_one(cur, sql: str, params: tuple, repeats: int = 20) -> dict:
    """Execute a parameterised query `repeats` times and return
    latency percentiles in microseconds."""
    # Warm-up: 3 runs discarded so the plan cache and OS page cache
    # reflect steady state.
    for _ in range(3):
        cur.execute(sql, params)
        cur.fetchall()
    samples = []
    for _ in range(repeats):
        t0 = time.perf_counter_ns()
        cur.execute(sql, params)
        rows = cur.fetchall()
        elapsed = (time.perf_counter_ns() - t0) / 1_000  # µs
        samples.append(elapsed)
    samples.sort()
    return {
        "sql": sql,
        "params": list(params),
        "row_count": len(rows),
        "p50_us": samples[len(samples) // 2],
        "p95_us": samples[int(len(samples) * 0.95)],
        "p99_us": samples[int(len(samples) * 0.99)],
        "min_us": samples[0],
        "max_us": samples[-1],
    }


def seed(cur, rows: int) -> None:
    """Create the bench schema and populate `rows` rows. Idempotent —
    drops existing tables first."""
    cur.execute("DROP TABLE IF EXISTS bench_leads")
    cur.execute("DROP TABLE IF EXISTS bench_companies")
    cur.execute("CREATE TABLE bench_companies (id SERIAL PRIMARY KEY, name TEXT)")
    cur.execute(
        "CREATE TABLE bench_leads ("
        "id SERIAL PRIMARY KEY, "
        "company_id INT, "
        "created_at BIGINT, "
        "label TEXT)"
    )
    for i in range(100):
        cur.execute("INSERT INTO bench_companies (name) VALUES (%s)", (f"company-{i}",))
    # Bulk-insert leads. One statement per row since not every target
    # supports multi-row VALUES syntax identically; we trade some
    # insert time for portability.
    t0 = time.perf_counter()
    for i in range(rows):
        cur.execute(
            "INSERT INTO bench_leads (company_id, created_at, label) VALUES (%s, %s, %s)",
            ((i % 100) + 1, 1_000_000 + i, f"lead-{i}"),
        )
    print(f"  seeded {rows} rows in {time.perf_counter() - t0:.1f}s", file=sys.stderr)


def run_bench(conn_kwargs: dict, engine_name: str, row_count: int) -> dict:
    """Connect, seed, and run every pagination shape; return a JSON-ready
    result dict."""
    conn = psycopg.connect(**conn_kwargs, autocommit=True)
    cur = conn.cursor()
    print(f"[{engine_name}] seeding {row_count} rows...", file=sys.stderr)
    seed(cur, row_count)

    shapes = {}

    # --- 1. Offset pagination at varying depths ---
    for offset in [0, 100, 1_000, 10_000, row_count - 10]:
        if offset >= row_count:
            continue
        shapes[f"offset_{offset}"] = bench_one(
            cur,
            "SELECT id, label FROM bench_leads ORDER BY id LIMIT %s OFFSET %s",
            (10, offset),
        )

    # --- 2. Keyset pagination at varying depths ---
    # For a fair comparison, we pass the same logical cursor as the
    # offset shapes (id at position 0, 100, 1000, ...).
    for after_id in [1, 101, 1_001, 10_001, row_count - 10]:
        if after_id >= row_count:
            continue
        shapes[f"keyset_after_{after_id}"] = bench_one(
            cur,
            "SELECT id, label FROM bench_leads WHERE id > %s ORDER BY id LIMIT %s",
            (after_id, 10),
        )

    # --- 3. Join + offset pagination ---
    for offset in [0, 1_000, 10_000]:
        if offset >= row_count:
            continue
        shapes[f"join_offset_{offset}"] = bench_one(
            cur,
            "SELECT l.id, l.label, c.name "
            "FROM bench_leads l LEFT OUTER JOIN bench_companies c "
            "ON l.company_id = c.id "
            "ORDER BY l.id LIMIT %s OFFSET %s",
            (10, offset),
        )

    # --- 4. Tuple keyset (Markon's canonical shape) ---
    shapes["tuple_keyset_mid"] = bench_one(
        cur,
        "SELECT id, created_at FROM bench_leads "
        "WHERE (created_at, id) < (%s, %s) "
        "ORDER BY created_at DESC, id DESC LIMIT %s",
        (1_000_000 + row_count // 2, row_count // 2, 10),
    )

    conn.close()
    return {
        "engine": engine_name,
        "row_count": row_count,
        "shapes": shapes,
    }


# ─────────────────────────────────────────────────────────────────────────────
# Reporting
# ─────────────────────────────────────────────────────────────────────────────

def print_table(result: dict) -> None:
    """Render one engine's results as a table."""
    print()
    print(f"=== {result['engine']} ({result['row_count']:,} rows) ===")
    print(f"{'shape':<30} {'p50':>8} {'p95':>8} {'p99':>8}  {'rows':>5}")
    print("-" * 66)
    for name, s in result["shapes"].items():
        print(
            f"{name:<30} {s['p50_us']:>6.0f}µs {s['p95_us']:>6.0f}µs "
            f"{s['p99_us']:>6.0f}µs  {s['row_count']:>5}"
        )


def print_compare(results: list[dict]) -> None:
    """Side-by-side p50 comparison across engines."""
    if not results:
        return
    shapes = list(results[0]["shapes"].keys())
    widths = max(len(r["engine"]) for r in results) + 2
    print()
    header = f"{'shape':<30}" + "".join(f"{r['engine']:>{widths}}" for r in results)
    print(header)
    print("-" * len(header))
    for name in shapes:
        row = f"{name:<30}"
        baseline = None
        for r in results:
            v = r["shapes"].get(name)
            if v is None:
                row += f"{'—':>{widths}}"
            else:
                row += f"{v['p50_us']:>{widths - 3}.0f}µs"
                if baseline is None:
                    baseline = v["p50_us"]
        print(row)


# ─────────────────────────────────────────────────────────────────────────────
# Entry point
# ─────────────────────────────────────────────────────────────────────────────

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[1])
    ap.add_argument("--host", default="127.0.0.1", help="PG host (or /tmp for Unix socket)")
    ap.add_argument("--port", type=int, default=5432)
    ap.add_argument("--user", default="postgres")
    ap.add_argument("--password", default="")
    ap.add_argument("--dbname", default="postgres")
    ap.add_argument("--name", default="target", help="Engine label in the report")
    ap.add_argument("--rows", type=int, default=10_000, help="Table size (default 10 000)")
    ap.add_argument("--out", type=Path, help="Write JSON report to this path")
    ap.add_argument(
        "--compare",
        nargs="+",
        type=Path,
        help="Merge and render existing JSON result files side-by-side (no measurement)",
    )
    args = ap.parse_args()

    if args.compare:
        results = [json.loads(p.read_text()) for p in args.compare]
        for r in results:
            print_table(r)
        print_compare(results)
        return 0

    conn_kwargs = dict(
        host=args.host,
        port=args.port,
        user=args.user,
        dbname=args.dbname,
    )
    if args.password:
        conn_kwargs["password"] = args.password

    result = run_bench(conn_kwargs, args.name, args.rows)
    print_table(result)

    if args.out:
        args.out.write_text(json.dumps(result, indent=2))
        print(f"\nWrote {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
