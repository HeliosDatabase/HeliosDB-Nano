#!/usr/bin/env python3
"""Triggers over the PostgreSQL EXTENDED query protocol (Parse/Bind/Execute).

WHY THIS FILE SPEAKS THE RAW PROTOCOL. psycopg2 interpolates parameters
CLIENT-side and sends everything over the SIMPLE query protocol, so it can never
reach the params executor family — which is exactly the family that used to
reject `CREATE TRIGGER` outright with

    Operator not yet implemented: CreateTrigger { … }

and that had no BEFORE-row trigger hook at all. This test therefore reuses the
stdlib PostgreSQL v3 client from `test_extended_describe.py` (one client, not a
second copy) to issue real Parse/Bind/Execute messages.

WHAT IT PROVES:
  1. `CREATE TRIGGER` and `DROP TRIGGER` succeed over Parse/Bind/Execute.
  2. An INSERT with SERVER-SIDE BOUND PARAMETERS gets the BEFORE-INSERT
     `NEW.<col> = <expr>` rewrite — the same row a simple-query INSERT gets.
  3. A `RETURN NULL` body suppresses the row on this path too.
  4. `INSERT … RETURNING` over Bind/Execute returns the REWRITTEN value.

WHAT IT DELIBERATELY DOES NOT PROVE: that trigger BODIES execute. They do not.
A side-effecting body writes nothing, and this test asserts that.

Server assumed started with `--auth trust` (the gate cookbook config:
`heliosdb-nano start --auth trust --http-port 0 --port 20000 --data-dir <fresh>`).
Connection params from env, mirroring test_copy.py:
    HELIOS_PGHOST (localhost) HELIOS_PGPORT (20000) HELIOS_PGDB (heliosdb)
    HELIOS_PGUSER (test_user) HELIOS_PGPASS (test_pass)

Exit code 0 = all checks pass.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

# ONE raw-protocol client for the whole directory — do not fork a second copy.
from test_extended_describe import Pg  # noqa: E402

FAILS = []


def check(cond, msg):
    print(("✓ " if cond else "✗ ") + msg)
    if not cond:
        FAILS.append(msg)


def extended(pg, sql, params=None, name="trg_ps"):
    """Run `sql` through Parse + Bind + Execute (the extended protocol)."""
    pg.describe_statement(sql, name=name)
    return pg.bind_exec(name, params or [])


def connect():
    return Pg(
        os.environ.get("HELIOS_PGHOST", "localhost"),
        int(os.environ.get("HELIOS_PGPORT", "20000")),
        os.environ.get("HELIOS_PGUSER", "test_user"),
        os.environ.get("HELIOS_PGDB", "heliosdb"),
        os.environ.get("HELIOS_PGPASS", "test_pass"),
    )


def main():
    pg = connect()

    # ---- fixtures over the simple protocol -------------------------------
    pg.simple("DROP TABLE IF EXISTS wtrg_t")
    pg.simple("DROP TABLE IF EXISTS wtrg_audit")
    pg.simple("CREATE TABLE wtrg_t (id INT, tag TEXT)")
    pg.simple("CREATE TABLE wtrg_audit (note TEXT)")
    pg.simple(
        "CREATE FUNCTION wtrg_mut() RETURNS TRIGGER AS $$ "
        "BEGIN NEW.tag = 'set-by-trigger'; RETURN NEW; END $$ LANGUAGE plpgsql"
    )
    pg.simple(
        "CREATE FUNCTION wtrg_skip() RETURNS TRIGGER AS $$ "
        "BEGIN RETURN NULL; END $$ LANGUAGE plpgsql"
    )
    pg.simple(
        "CREATE FUNCTION wtrg_audit_fn() RETURNS TRIGGER AS $$ "
        "BEGIN INSERT INTO wtrg_audit (note) VALUES ('fired'); RETURN NEW; END $$ LANGUAGE plpgsql"
    )

    # ---- 1. CREATE TRIGGER over Parse/Bind/Execute ------------------------
    ddl_ok, ddl_err = True, ""
    try:
        extended(
            pg,
            "CREATE TRIGGER wtrg_before BEFORE INSERT ON wtrg_t "
            "FOR EACH ROW EXECUTE FUNCTION wtrg_mut()",
            name="ps_create_trg",
        )
    except RuntimeError as e:  # the old failure mode
        ddl_ok, ddl_err = False, str(e)
    check(ddl_ok, "CREATE TRIGGER succeeds over the extended protocol: " + ddl_err)
    check(
        "not yet implemented" not in ddl_err,
        "the 'Operator not yet implemented: CreateTrigger' error is gone",
    )

    # ---- 2. bound-parameter INSERT gets the rewrite -----------------------
    extended(
        pg,
        "INSERT INTO wtrg_t (id, tag) VALUES ($1, $2)",
        params=["1", "original"],
        name="ps_ins",
    )
    rows = pg.simple("SELECT tag FROM wtrg_t WHERE id = 1")
    check(
        rows == [["set-by-trigger"]],
        f"bound-parameter INSERT is rewritten by the BEFORE-INSERT trigger: {rows}",
    )

    # ---- 3. INSERT … RETURNING over Bind/Execute reflects the rewrite -----
    ret = extended(
        pg,
        "INSERT INTO wtrg_t (id, tag) VALUES ($1, $2) RETURNING tag",
        params=["2", "original"],
        name="ps_ins_ret",
    )
    check(
        ret == [["set-by-trigger"]],
        f"RETURNING over the extended protocol reflects the rewritten row: {ret}",
    )

    # ---- 4. trigger BODIES still do not execute ---------------------------
    pg.simple("DROP TRIGGER wtrg_before ON wtrg_t")
    pg.simple(
        "CREATE TRIGGER wtrg_audit_trg BEFORE INSERT ON wtrg_t "
        "FOR EACH ROW EXECUTE FUNCTION wtrg_audit_fn()"
    )
    extended(
        pg,
        "INSERT INTO wtrg_t (id, tag) VALUES ($1, $2)",
        params=["3", "kept"],
        name="ps_ins_audit",
    )
    audit = pg.simple("SELECT note FROM wtrg_audit")
    check(audit == [], f"a side-effecting trigger body still does NOT run: {audit}")
    kept = pg.simple("SELECT tag FROM wtrg_t WHERE id = 3")
    check(kept == [["kept"]], f"the row itself is written unchanged: {kept}")
    pg.simple("DROP TRIGGER wtrg_audit_trg ON wtrg_t")

    # ---- 5. RETURN NULL suppresses on this path too -----------------------
    pg.simple(
        "CREATE TRIGGER wtrg_skip_trg BEFORE INSERT ON wtrg_t "
        "FOR EACH ROW EXECUTE FUNCTION wtrg_skip()"
    )
    extended(
        pg,
        "INSERT INTO wtrg_t (id, tag) VALUES ($1, $2)",
        params=["42", "gone"],
        name="ps_ins_skip",
    )
    skipped = pg.simple("SELECT id FROM wtrg_t WHERE id = 42")
    check(skipped == [], f"RETURN NULL suppresses the row over the extended protocol: {skipped}")

    # ---- 6. DROP TRIGGER over Parse/Bind/Execute --------------------------
    drop_ok, drop_err = True, ""
    try:
        extended(pg, "DROP TRIGGER wtrg_skip_trg ON wtrg_t", name="ps_drop_trg")
    except RuntimeError as e:
        drop_ok, drop_err = False, str(e)
    check(drop_ok, "DROP TRIGGER succeeds over the extended protocol: " + drop_err)

    extended(
        pg,
        "INSERT INTO wtrg_t (id, tag) VALUES ($1, $2)",
        params=["43", "after-drop"],
        name="ps_ins_after_drop",
    )
    after = pg.simple("SELECT tag FROM wtrg_t WHERE id = 43")
    check(after == [["after-drop"]], f"after DROP TRIGGER the row is untouched: {after}")

    # ---- cleanup ----------------------------------------------------------
    pg.simple("DROP TABLE IF EXISTS wtrg_t")
    pg.simple("DROP TABLE IF EXISTS wtrg_audit")
    pg.simple("DROP FUNCTION IF EXISTS wtrg_mut")
    pg.simple("DROP FUNCTION IF EXISTS wtrg_skip")
    pg.simple("DROP FUNCTION IF EXISTS wtrg_audit_fn")
    pg.close()

    print()
    if FAILS:
        print(f"FAILED: {len(FAILS)} check(s)")
        for m in FAILS:
            print("  - " + m)
        sys.exit(1)
    print("ALL EXTENDED-PROTOCOL TRIGGER TESTS PASSED")


if __name__ == "__main__":
    main()
