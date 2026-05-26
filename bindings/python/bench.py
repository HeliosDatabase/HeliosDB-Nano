"""Hot-path benchmark for the in-process PyO3 binding, run against a real
HeliosDB-Nano data dir. Mirrors the four queries from issue #1 and prints
in-process latency next to the Python-accessible numbers reported there.

Usage: python bench.py [DATA_DIR]   (default: /tmp/td-bench)
"""
import statistics
import sys
import time

import heliosdb_nano

DATA = sys.argv[1] if len(sys.argv) > 1 else "/tmp/td-bench"
N = 25


def bench(db, label, sql):
    db.query(sql)  # warm
    samples = []
    for _ in range(N):
        t = time.perf_counter()
        rows = db.query(sql)
        samples.append((time.perf_counter() - t) * 1000.0)
    return label, min(samples), statistics.median(samples), rows


def main():
    db = heliosdb_nano.EmbeddedDatabase(DATA)
    cols = list(db.query("SELECT * FROM dashboard.messages LIMIT 1")[0].keys())
    print(f"data dir: {DATA}")
    print(f"columns:  {cols}\n")

    group_col = "project_slug" if "project_slug" in cols else "type"
    queries = [
        ("COUNT(*)", "SELECT COUNT(*) AS n FROM dashboard.messages"),
        ("COUNT(DISTINCT session_id)", "SELECT COUNT(DISTINCT session_id) AS n FROM dashboard.messages"),
        ("WHERE type='user'", "SELECT COUNT(*) AS n FROM dashboard.messages WHERE type = 'user'"),
        (f"GROUP BY {group_col}, SUM", f"SELECT {group_col}, SUM(input_tokens) AS s FROM dashboard.messages GROUP BY {group_col}"),
    ]

    # Published Python-accessible numbers from issue #1 (ms): PG-wire, REPL floor, sqlite.
    published = {
        "COUNT(*)": (715, 309, 1.7),
        "COUNT(DISTINCT session_id)": (1489, 309, 27),
        "WHERE type='user'": (1312, 309, 76),
        f"GROUP BY {group_col}, SUM": (1439, 309, 145),
    }

    print(f"{'query':<30} {'in-proc min':>11} {'median':>9}   {'PG-wire':>8} {'REPL':>6} {'sqlite':>7}   {'vs REPL':>8}")
    print("-" * 96)
    for label, sql in queries:
        _, mn, med, rows = bench(db, label, sql)
        pg, repl, sl = published.get(label, (None, None, None))
        speedup = f"{repl / med:.0f}x" if repl and med > 0 else "-"
        pg_s = f"{pg}" if pg else "-"
        repl_s = f"{repl}" if repl else "-"
        sl_s = f"{sl}" if sl else "-"
        print(f"{label:<30} {mn:>9.2f}ms {med:>7.2f}ms   {pg_s:>8} {repl_s:>6} {sl_s:>7}   {speedup:>8}")

    # Correctness against known ground truth (issue #2's data dir).
    n = db.query("SELECT COUNT(*) AS n FROM dashboard.messages")[0]["n"]
    d = db.query("SELECT COUNT(DISTINCT session_id) AS n FROM dashboard.messages")[0]["n"]
    print(f"\ncorrectness: COUNT(*)={n} (expect 448573), COUNT(DISTINCT session_id)={d} (expect 265)")
    assert n == 448573 and d == 265, "ground-truth mismatch"
    print("OK")


if __name__ == "__main__":
    main()
