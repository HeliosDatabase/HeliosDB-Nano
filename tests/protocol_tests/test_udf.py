#!/usr/bin/env python3
"""User-defined function / procedure tests over the PostgreSQL wire.

SCOPE — read this before trusting the file. psycopg2 interpolates parameters
CLIENT-side and sends everything over the SIMPLE query protocol (see the header
of test_extended_describe.py), so this file exercises the wire's simple-query
route and the server-side PREPARE/EXECUTE route. It does NOT exercise the
Parse/Bind/Execute extended protocol; the PARAMS executor family that the
extended protocol routes into is covered by `tests/udf_invocation_tests.rs`
(`db.execute_params` / `db.query_params`), and a raw-protocol extended test for
UDFs is a named follow-up.

What it proves that no Rust test can: a UDF registered and invoked through the
real PostgreSQL listener behaves the same as through the embedded API.

    HELIOS_PGHOST (localhost) HELIOS_PGPORT (20000) HELIOS_PGDB (heliosdb)
    HELIOS_PGUSER (test_user) HELIOS_PGPASS (test_pass)

Exit code 0 = all pass. Each check prints ✓/✗.
"""
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


def main():
    conn = connect()
    conn.autocommit = True
    cur = conn.cursor()

    cur.execute("DROP FUNCTION IF EXISTS w_dbl")
    cur.execute("DROP PROCEDURE IF EXISTS w_log")
    cur.execute("DROP TABLE IF EXISTS w_audit")
    cur.execute("DROP TABLE IF EXISTS w_rows")
    cur.execute("CREATE TABLE w_audit (id INT, note TEXT)")
    cur.execute("CREATE TABLE w_rows (id INT)")
    cur.execute("INSERT INTO w_rows (id) VALUES (1), (2), (3)")

    # ---- CREATE FUNCTION over the wire, then call it ----
    cur.execute(
        "CREATE FUNCTION w_dbl(x INTEGER) RETURNS INTEGER "
        "AS $$ SELECT $1 * 2 $$ LANGUAGE sql"
    )
    cur.execute("SELECT w_dbl(21)")
    row = cur.fetchone()
    check(row is not None and int(row[0]) == 42, "SELECT w_dbl(21) = 42")

    cur.execute("SELECT public.w_dbl(21)")
    row = cur.fetchone()
    check(row is not None and int(row[0]) == 42, "SELECT public.w_dbl(21) = 42")

    # ---- Per-row projection and a WHERE-clause call ----
    cur.execute("SELECT id, w_dbl(id) FROM w_rows ORDER BY id")
    got = [(int(a), int(b)) for a, b in cur.fetchall()]
    check(got == [(1, 2), (2, 4), (3, 6)], "per-row UDF projection = %r" % (got,))

    cur.execute("SELECT id FROM w_rows WHERE w_dbl(id) = 4")
    got = [int(r[0]) for r in cur.fetchall()]
    check(got == [2], "UDF in WHERE filters = %r" % (got,))

    # ---- Server-side PREPARE/EXECUTE of a UDF call ----
    cur.execute("PREPARE w_p AS SELECT w_dbl(7)")
    cur.execute("EXECUTE w_p")
    row = cur.fetchone()
    check(row is not None and int(row[0]) == 14, "PREPARE/EXECUTE of a UDF call = 14")

    # ---- CREATE PROCEDURE over the wire, then CALL ----
    cur.execute(
        "CREATE PROCEDURE w_log(p_id INTEGER) LANGUAGE sql "
        "AS $$INSERT INTO w_audit VALUES ($p_id, 'wire')$$"
    )
    cur.execute("CALL w_log(1)")
    cur.execute("CALL w_log(2)")
    cur.execute("SELECT id FROM w_audit ORDER BY id")
    ids = [int(r[0]) for r in cur.fetchall()]
    check(ids == [1, 2], "CALL ran the procedure body over the wire = %r" % (ids,))

    # ---- Negative: an unknown function raises, it does not return NULL ----
    try:
        cur.execute("SELECT w_never_defined(1)")
        cur.fetchall()
        check(False, "an unknown function must raise")
    except psycopg2.Error as e:
        check("Unknown scalar function" in str(e), "unknown function raises: %s" % str(e).strip())

    # ---- Negative: set-returning use is still a missing table ----
    try:
        cur.execute("SELECT * FROM w_dbl(1)")
        cur.fetchall()
        check(False, "SELECT * FROM f() must still fail")
    except psycopg2.Error as e:
        check("does not exist" in str(e), "SELECT * FROM f() still fails: %s" % str(e).strip())

    # ---- DROP FUNCTION over the wire really drops it ----
    cur.execute("DROP FUNCTION w_dbl")
    try:
        cur.execute("SELECT w_dbl(1)")
        cur.fetchall()
        check(False, "a dropped function must raise")
    except psycopg2.Error as e:
        check("Unknown scalar function" in str(e), "dropped function raises: %s" % str(e).strip())

    cur.close()
    conn.close()

    if FAILS:
        print("\n%d check(s) FAILED:" % len(FAILS))
        for f in FAILS:
            print("  - " + f)
        return 1
    print("\nall UDF wire checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
