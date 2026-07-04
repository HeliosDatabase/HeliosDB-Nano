#!/usr/bin/env python3
"""COPY FROM STDIN wire tests for HeliosDB-Nano (R-B1 typed bulk path + R-B4
session-transaction COPY). Self-contained; connection params from env so the
gate can point it at any running server.

    HELIOS_PGHOST (localhost) HELIOS_PGPORT (20000) HELIOS_PGDB (heliosdb)
    HELIOS_PGUSER (test_user) HELIOS_PGPASS (test_pass)

Exit code 0 = all pass. Each check prints ✓/✗.
"""
import io
import os
import sys

import psycopg2

FAILS = []


def check(cond, msg):
    print(("✓ " if cond else "✗ ") + msg)
    if not cond:
        FAILS.append(msg)


def connect():
    return psycopg2.connect(
        host=os.environ.get("HELIOS_PGHOST", "localhost"),
        port=int(os.environ.get("HELIOS_PGPORT", "20000")),
        database=os.environ.get("HELIOS_PGDB", "heliosdb"),
        user=os.environ.get("HELIOS_PGUSER", "test_user"),
        password=os.environ.get("HELIOS_PGPASS", "test_pass"),
        sslmode="disable",
    )


def csv_buf(rows):
    return io.StringIO("".join(",".join(r) + "\n" for r in rows))


def main():
    conn = connect()
    conn.autocommit = True
    cur = conn.cursor()

    # ---- Happy path: large CSV COPY round-trips (exercises the fast batch) ----
    cur.execute("DROP TABLE IF EXISTS copy_t")
    cur.execute("CREATE TABLE copy_t (id INT PRIMARY KEY, v INT)")
    n = 20000
    buf = csv_buf([[str(i), str((i * 7) % 100000)] for i in range(1, n + 1)])
    cur.copy_expert("COPY copy_t (id, v) FROM STDIN WITH (FORMAT csv)", buf)
    cur.execute("SELECT count(*) FROM copy_t")
    check(cur.fetchone()[0] == n, f"COPY loaded all {n} rows")
    cur.execute("SELECT v FROM copy_t WHERE id = 12345")
    check(cur.fetchone()[0] == (12345 * 7) % 100000, "COPY value round-trips correctly")

    # ---- Atomicity: a duplicate PK rejects the whole COPY (0 rows added) ----
    cur.execute("DROP TABLE IF EXISTS copy_atom")
    cur.execute("CREATE TABLE copy_atom (id INT PRIMARY KEY, v INT)")
    cur.execute("INSERT INTO copy_atom VALUES (5, 500)")
    bad = csv_buf([["1", "10"], ["5", "50"], ["2", "20"]])  # 5 collides
    try:
        cur.copy_expert("COPY copy_atom FROM STDIN WITH (FORMAT csv)", bad)
        check(False, "duplicate-PK COPY should raise")
    except psycopg2.Error:
        conn.rollback()
        check(True, "duplicate-PK COPY raised")
    cur.execute("SELECT count(*) FROM copy_atom")
    check(cur.fetchone()[0] == 1, "atomic COPY left only the pre-existing row")

    # ---- R-B4: COPY inside a transaction participates in ROLLBACK/COMMIT ----
    conn.autocommit = False
    cur.execute("DROP TABLE IF EXISTS copy_txn")
    cur.execute("CREATE TABLE copy_txn (id INT PRIMARY KEY, v INT)")
    conn.commit()

    cur.copy_expert("COPY copy_txn FROM STDIN WITH (FORMAT csv)", csv_buf([["1", "1"], ["2", "2"]]))
    conn.rollback()
    cur.execute("SELECT count(*) FROM copy_txn")
    check(cur.fetchone()[0] == 0, "R-B4: COPY rows roll back with the transaction")
    conn.commit()

    cur.copy_expert("COPY copy_txn FROM STDIN WITH (FORMAT csv)", csv_buf([["3", "3"], ["4", "4"]]))
    conn.commit()
    cur.execute("SELECT count(*) FROM copy_txn")
    check(cur.fetchone()[0] == 2, "R-B4: COPY rows commit with the transaction")
    conn.rollback()

    # ---- Fallback path still works: FK table takes the generic route ----
    conn.autocommit = True
    cur.execute("DROP TABLE IF EXISTS copy_child")
    cur.execute("DROP TABLE IF EXISTS copy_parent")
    cur.execute("CREATE TABLE copy_parent (id INT PRIMARY KEY)")
    cur.execute("CREATE TABLE copy_child (id INT PRIMARY KEY, pid INT REFERENCES copy_parent(id))")
    cur.execute("INSERT INTO copy_parent VALUES (1)")
    cur.copy_expert("COPY copy_child FROM STDIN WITH (FORMAT csv)", csv_buf([["10", "1"], ["11", "1"]]))
    cur.execute("SELECT count(*) FROM copy_child")
    check(cur.fetchone()[0] == 2, "FK table COPY works via fallback path")

    cur.close()
    conn.close()

    print()
    if FAILS:
        print(f"FAILED: {len(FAILS)} check(s)")
        for m in FAILS:
            print(f"  - {m}")
        sys.exit(1)
    print("ALL COPY WIRE TESTS PASSED")


if __name__ == "__main__":
    main()
