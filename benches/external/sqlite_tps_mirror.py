#!/usr/bin/env python3
"""SQLite mirror of tests/tps_workloads.rs — identical workloads, for head-to-head TPS.

Usage:
  SQLITE_TPS_MODE=mem   SQLITE_TPS_N=50000 python3 sqlite_tps_mirror.py
  SQLITE_TPS_MODE=disk  SQLITE_TPS_N=50000 python3 sqlite_tps_mirror.py   # journal=DELETE, sync=FULL (durable default)
  SQLITE_TPS_MODE=wal   SQLITE_TPS_N=50000 python3 sqlite_tps_mirror.py   # WAL, sync=NORMAL (common prod)
"""
import os, sqlite3, time, tempfile, shutil

MODE = os.environ.get("SQLITE_TPS_MODE", "mem")
N = int(os.environ.get("SQLITE_TPS_N", "50000"))
M = int(os.environ.get("SQLITE_TPS_M", str(max(N // 5, 2000))))

def open_db(tmpdir):
    if MODE == "mem":
        c = sqlite3.connect(":memory:")
    else:
        c = sqlite3.connect(os.path.join(tmpdir, "bench.db"))
    if MODE == "disk":
        c.execute("PRAGMA journal_mode=DELETE")
        c.execute("PRAGMA synchronous=FULL")
    elif MODE == "wal":
        c.execute("PRAGMA journal_mode=WAL")
        c.execute("PRAGMA synchronous=NORMAL")
    return c

def bench(label, ops, fn):
    t = time.perf_counter()
    fn()
    secs = time.perf_counter() - t
    print(f"{label:<28} {ops:>10} ops  {secs:>9.3f} s  {ops/secs:>14.0f} ops/s  {secs*1e6/ops:>10.2f} us/op")

def main():
    tmp = tempfile.mkdtemp(prefix="sqlite_tps_")
    print("\n================ SQLite TPS suite ================")
    print(f"mode={MODE}  N={N}  M={M}  sqlite={sqlite3.sqlite_version}")
    print("-" * 80)
    c = open_db(tmp)
    cur = c.cursor()
    cur.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT, age INTEGER, balance INTEGER)")
    cur.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, amount INTEGER, status TEXT)")
    c.commit()

    def bulk_users():
        cur.execute("BEGIN")
        for i in range(N):
            cur.execute("INSERT INTO users (id,name,email,age,balance) VALUES (?,?,?,?,?)",
                        (i, f"User{i}", f"u{i}@ex.com", 18 + (i % 60), (i*7) % 100000))
        c.commit()
    bench("bulk_insert_users(txn)", N, bulk_users)

    cur.execute("BEGIN")
    for i in range(N*2):
        cur.execute("INSERT INTO orders (id,user_id,amount,status) VALUES (?,?,?,?)",
                    (i, i % N, (i*13) % 5000, "paid" if i % 3 == 0 else "pending"))
    c.commit()

    def autocommit_insert():
        for i in range(M):
            idx = N + i
            cur.execute("INSERT INTO users (id,name,email,age,balance) VALUES (?,?,?,?,?)",
                        (idx, f"AC{idx}", f"ac{idx}@ex.com", 33, 500))
            c.commit()
    bench("autocommit_insert", M, autocommit_insert)

    def point_lookup():
        for i in range(M):
            idx = (i * 2654435761) % N
            cur.execute("SELECT * FROM users WHERE id = ?", (idx,)).fetchall()
    bench("point_lookup_pk", M, point_lookup)

    def point_lookup_hot():
        for _ in range(M):
            cur.execute("SELECT * FROM users WHERE id = 12345").fetchall()
    bench("point_lookup_hot", M, point_lookup_hot)

    def update_by_pk():
        for i in range(M):
            idx = (i * 40503) % N
            cur.execute("UPDATE users SET balance = balance + 1 WHERE id = ?", (idx,))
            c.commit()
    bench("update_by_pk", M, update_by_pk)

    def delete_by_pk():
        for i in range(M):
            cur.execute("DELETE FROM users WHERE id = ?", (N + i,))
            c.commit()
    bench("delete_by_pk", M, delete_by_pk)

    SCAN = 20
    def filter_scan():
        for _ in range(SCAN):
            cur.execute("SELECT id, name FROM users WHERE age > 50").fetchall()
    bench("filter_scan(age>50)", SCAN, filter_scan)

    def agg():
        for _ in range(SCAN):
            cur.execute("SELECT COUNT(*), SUM(balance), AVG(age) FROM users").fetchall()
    bench("agg_count_sum_avg", SCAN, agg)

    def group_by():
        for _ in range(SCAN):
            cur.execute("SELECT status, COUNT(*), SUM(amount) FROM orders GROUP BY status").fetchall()
    bench("group_by_status", SCAN, group_by)

    JOIN = 10
    def join():
        for _ in range(JOIN):
            cur.execute("SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id=o.user_id "
                        "WHERE o.status='paid' AND u.age>40").fetchall()
    bench("join_users_orders", JOIN, join)

    def order_limit():
        for _ in range(SCAN):
            cur.execute("SELECT id, balance FROM users ORDER BY balance DESC LIMIT 10").fetchall()
    bench("order_by_limit10", SCAN, order_limit)

    print("-" * 80)
    c.close()
    shutil.rmtree(tmp, ignore_errors=True)

if __name__ == "__main__":
    main()
