#!/usr/bin/env python3
"""W2.3 — extended-protocol Parse reuse / Describe OID-parity (wire-level).

psycopg2 interpolates parameters CLIENT-side and sends everything over the
SIMPLE query protocol, so it NEVER exercises the extended Parse / Describe path
that W2.3 changes. This test therefore speaks the PostgreSQL v3 wire protocol
directly (Python stdlib only — no new dependency) to issue real
`Parse` + `Describe(statement)` messages and assert the `RowDescription`
(column names + pg_type OIDs) the server returns BEFORE any `Bind` — the exact
"Describe-before-Bind" metadata every extended-protocol driver (psycopg3, node
`pg`, `postgres-js`, JDBC) consumes at prepare time.

OID-parity is THE contract W2.3 must preserve while sourcing the Describe schema
from the shared parameterized plan cache instead of a throwaway private plan:
    numeric  -> 1700   (the 3.58.3 regression class — MUST stay 1700)
    text     -> 25     int4 -> 23     int8 -> 20     varchar -> 1043

Server assumed started with `--auth trust` (the gate cookbook config:
`heliosdb-nano start --auth trust --http-port 0 --port 20000 --data-dir <fresh>`).
Connection params from env, mirroring test_copy.py:
    HELIOS_PGHOST (localhost) HELIOS_PGPORT (20000) HELIOS_PGDB (heliosdb)
    HELIOS_PGUSER (test_user) HELIOS_PGPASS (test_pass)

Exit code 0 = all checks pass.
"""
import hashlib
import os
import socket
import struct
import sys

FAILS = []


def check(cond, msg):
    print(("✓ " if cond else "✗ ") + msg)
    if not cond:
        FAILS.append(msg)


# --- minimal PostgreSQL v3 wire client (stdlib only) -----------------------


class Pg:
    def __init__(self, host, port, user, database, password):
        self.sock = socket.create_connection((host, port), timeout=20)
        self.buf = b""
        self._startup(user, database, password)

    def _send(self, type_byte, payload):
        # type_byte == b"" for the untyped StartupMessage.
        self.sock.sendall(type_byte + struct.pack("!i", len(payload) + 4) + payload)

    def _recv_exact(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("server closed the connection")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def recv_msg(self):
        hdr = self._recv_exact(5)
        length = struct.unpack("!i", hdr[1:5])[0]
        return hdr[0:1], self._recv_exact(length - 4)

    def _startup(self, user, database, password):
        params = b"user\x00" + user.encode() + b"\x00database\x00" + database.encode() + b"\x00\x00"
        self._send(b"", struct.pack("!i", 196608) + params)  # protocol 3.0
        while True:
            mtype, payload = self.recv_msg()
            if mtype == b"R":
                code = struct.unpack("!i", payload[0:4])[0]
                if code == 0:  # AuthenticationOk
                    continue
                if code == 3:  # cleartext password
                    self._send(b"p", password.encode() + b"\x00")
                elif code == 5:  # md5
                    salt = payload[4:8]
                    inner = hashlib.md5(password.encode() + user.encode()).hexdigest().encode()
                    token = b"md5" + hashlib.md5(inner + salt).hexdigest().encode()
                    self._send(b"p", token + b"\x00")
                else:
                    raise RuntimeError(
                        f"unsupported auth code {code}; start the test server with "
                        "--auth trust (or password/md5)"
                    )
            elif mtype == b"Z":  # ReadyForQuery
                return
            elif mtype == b"E":
                raise RuntimeError("startup error: " + _err_text(payload))
            # ignore ParameterStatus (S), BackendKeyData (K), Notice (N)

    def simple(self, sql):
        """Simple-protocol statement; drains to ReadyForQuery, returns text rows."""
        self._send(b"Q", sql.encode() + b"\x00")
        rows, err = [], None
        while True:
            mtype, payload = self.recv_msg()
            if mtype == b"Z":
                break
            if mtype == b"D":
                rows.append(_decode_datarow(payload))
            elif mtype == b"E":
                err = _err_text(payload)
        if err:
            raise RuntimeError("simple query error: " + err)
        return rows

    def describe_statement(self, sql, name=""):
        """Parse + Describe(statement) + Sync. Returns (param_oids, fields) where
        fields == [(colname, type_oid), ...], captured BEFORE any Bind. `fields`
        is [] when the server answers NoData."""
        parse = name.encode() + b"\x00" + sql.encode() + b"\x00" + struct.pack("!h", 0)
        self._send(b"P", parse)
        self._send(b"D", b"S" + name.encode() + b"\x00")
        self._send(b"S", b"")
        params, fields, err = [], None, None
        while True:
            mtype, payload = self.recv_msg()
            if mtype == b"Z":
                break
            elif mtype == b"t":  # ParameterDescription
                (n,) = struct.unpack("!h", payload[0:2])
                params = [struct.unpack("!i", payload[2 + 4 * i:6 + 4 * i])[0] for i in range(n)]
            elif mtype == b"T":  # RowDescription
                fields = _decode_rowdesc(payload)
            elif mtype == b"n":  # NoData
                fields = []
            elif mtype == b"E":
                err = _err_text(payload)
        if err:
            raise RuntimeError("describe error: " + err)
        return params, fields

    def bind_exec(self, stmt_name, param_texts, portal=""):
        """Bind text params to a prepared statement + Execute + Sync; text rows."""
        b = portal.encode() + b"\x00" + stmt_name.encode() + b"\x00"
        b += struct.pack("!h", 0)  # 0 parameter format codes -> all text
        b += struct.pack("!h", len(param_texts))
        for p in param_texts:
            if p is None:
                b += struct.pack("!i", -1)
            else:
                pb = p.encode()
                b += struct.pack("!i", len(pb)) + pb
        b += struct.pack("!h", 0)  # 0 result format codes -> all text
        self._send(b"B", b)
        self._send(b"E", portal.encode() + b"\x00" + struct.pack("!i", 0))
        self._send(b"S", b"")
        rows, err = [], None
        while True:
            mtype, payload = self.recv_msg()
            if mtype == b"Z":
                break
            if mtype == b"D":
                rows.append(_decode_datarow(payload))
            elif mtype == b"E":
                err = _err_text(payload)
        if err:
            raise RuntimeError("bind/exec error: " + err)
        return rows

    def close(self):
        try:
            self._send(b"X", b"")  # Terminate
        except OSError:
            pass
        self.sock.close()


def _err_text(payload):
    fields = payload.split(b"\x00")
    return " ".join(f[1:].decode("utf-8", "replace") for f in fields if f[:1] in (b"M", b"S", b"C"))


def _decode_rowdesc(payload):
    (nfields,) = struct.unpack("!h", payload[0:2])
    pos, out = 2, []
    for _ in range(nfields):
        end = payload.index(b"\x00", pos)
        name = payload[pos:end].decode()
        pos = end + 1
        # skip table_oid(i32) + column_attr_num(i16) -> data_type_oid(i32)
        type_oid = struct.unpack("!i", payload[pos + 6:pos + 10])[0]
        out.append((name, type_oid))
        pos += 18  # table_oid+col_attr+type_oid+size+modifier+format
    return out


def _decode_datarow(payload):
    (ncols,) = struct.unpack("!h", payload[0:2])
    pos, cols = 2, []
    for _ in range(ncols):
        (ln,) = struct.unpack("!i", payload[pos:pos + 4])
        pos += 4
        if ln < 0:
            cols.append(None)
        else:
            cols.append(payload[pos:pos + ln].decode("utf-8", "replace"))
            pos += ln
    return cols


def main():
    pg = Pg(
        os.environ.get("HELIOS_PGHOST", "localhost"),
        int(os.environ.get("HELIOS_PGPORT", "20000")),
        os.environ.get("HELIOS_PGUSER", "test_user"),
        os.environ.get("HELIOS_PGDB", "heliosdb"),
        os.environ.get("HELIOS_PGPASS", "test_pass"),
    )

    # --- fixtures (simple protocol) ---
    pg.simple("DROP TABLE IF EXISTS w23_acct")
    pg.simple("DROP TABLE IF EXISTS w23_dept")
    pg.simple("CREATE TABLE w23_dept (dept_id INT PRIMARY KEY, dept_name TEXT)")
    pg.simple(
        "CREATE TABLE w23_acct (id INT PRIMARY KEY, name TEXT, bal NUMERIC, "
        "big BIGINT, code VARCHAR(8), dept_id INT)"
    )
    pg.simple("INSERT INTO w23_dept VALUES (1,'eng'),(2,'ops')")
    pg.simple(
        "INSERT INTO w23_acct VALUES "
        "(1,'alice',10.50,100000,'AA',1),(2,'bob',20.25,200000,'BB',2),(3,'carol',30.00,300000,'CC',1)"
    )

    # --- 1) point read: Describe-before-Bind names + OIDs (numeric MUST be 1700) ---
    _, f = pg.describe_statement("SELECT id, name, bal, big, code FROM w23_acct WHERE id = $1")
    check(
        f == [("id", 23), ("name", 25), ("bal", 1700), ("big", 20), ("code", 1043)],
        f"point-read Describe names+OIDs (numeric=1700, varchar=1043): {f}",
    )

    # --- 2) join: projected column names + OIDs ---
    _, jf = pg.describe_statement(
        "SELECT a.id, a.name, d.dept_name FROM w23_acct a "
        "JOIN w23_dept d ON a.dept_id = d.dept_id WHERE a.id = $1"
    )
    check([o for _, o in (jf or [])] == [23, 25, 25], f"join Describe OIDs: {jf}")
    check([n for n, _ in (jf or [])] == ["id", "name", "dept_name"], f"join Describe names: {jf}")

    # --- 3) aggregate alias ---
    _, af = pg.describe_statement("SELECT count(*) AS n FROM w23_acct")
    check(af == [("n", 20)], f"aggregate alias Describe (count(*) AS n -> int8/20 named n): {af}")

    # --- 4) numeric column alone stays 1700 ---
    _, nf = pg.describe_statement("SELECT bal FROM w23_acct WHERE id = $1")
    check(nf == [("bal", 1700)], f"lone numeric column Describe stays 1700: {nf}")

    # --- 5) prepared reuse across differing literals: one Describe, many Binds ---
    _, rf = pg.describe_statement("SELECT id, bal FROM w23_acct WHERE id = $1", name="ps_reuse")
    check(
        rf == [("id", 23), ("bal", 1700)],
        f"prepared-reuse Describe metadata (fixed at prepare): {rf}",
    )
    r1 = pg.bind_exec("ps_reuse", ["1"], portal="pr1")
    r3 = pg.bind_exec("ps_reuse", ["3"], portal="pr3")
    check(
        len(r1) == 1 and r1[0][0] == "1" and abs(float(r1[0][1]) - 10.50) < 1e-9,
        f"prepared reuse literal=1 -> {r1}",
    )
    check(
        len(r3) == 1 and r3[0][0] == "3" and abs(float(r3[0][1]) - 30.00) < 1e-9,
        f"prepared reuse literal=3 -> {r3}",
    )

    # --- 6) regular-view DDL invalidates the shared plan cache feeding Describe ---
    # Regression: W2.3 sources the Describe schema from the parameterized plan
    # cache (keyed by SQL TEXT), into which regular views are inlined. DROP/
    # CREATE VIEW must invalidate that cache, else the SAME `SELECT * FROM v`
    # text Describes the STALE (pre-redefine) column set. Pre-fix
    # plan_invalidates_sql_caches omitted CreateView/DropView.
    pg.simple("DROP VIEW IF EXISTS w23_v")
    pg.simple("CREATE VIEW w23_v AS SELECT id FROM w23_acct")
    _, vb = pg.describe_statement("SELECT * FROM w23_v", name="ps_view_before")
    check(vb == [("id", 23)], f"view Describe before redefine (single int4 col id): {vb}")
    # Redefine the SAME view name with an extra TEXT column; the identical SQL
    # text must now Describe as two columns, not the stale one-column shape.
    pg.simple("DROP VIEW w23_v")
    pg.simple("CREATE VIEW w23_v AS SELECT id, name FROM w23_acct")
    _, va = pg.describe_statement("SELECT * FROM w23_v", name="ps_view_after")
    check(
        va == [("id", 23), ("name", 25)],
        f"view Describe after DROP+re-CREATE reflects new columns (id/int4, name/text): {va}",
    )
    pg.simple("DROP VIEW IF EXISTS w23_v")

    # --- 7) SAME regression, but the redefining DDL runs over the EXTENDED
    # protocol (Parse/Bind/Execute) — the route psycopg3 / JDBC / Npgsql use by
    # default (e.g. Alembic migrations). That route lands in the parameterized
    # execute funnel (execute_params -> execute_plan_with_params_inner catch-all
    # executor arm), which the simple-Q DDL gate exercised by check #6 does NOT
    # cover. Without a gate at the params funnel the CREATE OR REPLACE executes
    # but never clears the "\0params\0<sql>" plan cache, so the second Describe
    # of the SAME `SELECT * FROM v` text returns the STALE one-column shape.
    pg.simple("DROP VIEW IF EXISTS w23_ev")
    pg.simple("CREATE VIEW w23_ev AS SELECT id FROM w23_acct")
    _, evb = pg.describe_statement("SELECT * FROM w23_ev", name="ps_ev_before")
    check(evb == [("id", 23)], f"extended-DDL view Describe before redefine (single int4 col id): {evb}")
    # Redefine via Parse (+Describe→NoData) then Bind+Execute — the extended
    # DDL funnel, NOT simple Q.
    pg.describe_statement("CREATE OR REPLACE VIEW w23_ev AS SELECT id, name FROM w23_acct", name="ps_ev_ddl")
    pg.bind_exec("ps_ev_ddl", [], portal="ps_ev_ddl_p")
    _, eva = pg.describe_statement("SELECT * FROM w23_ev", name="ps_ev_after")
    check(
        eva == [("id", 23), ("name", 25)],
        f"extended-DDL view Describe after CREATE OR REPLACE via Parse/Bind/Execute "
        f"reflects new columns (id/int4, name/text): {eva}",
    )
    pg.simple("DROP VIEW IF EXISTS w23_ev")

    # cleanup
    pg.simple("DROP TABLE IF EXISTS w23_acct")
    pg.simple("DROP TABLE IF EXISTS w23_dept")
    pg.close()

    print()
    if FAILS:
        print(f"FAILED: {len(FAILS)} check(s)")
        for m in FAILS:
            print("  - " + m)
        sys.exit(1)
    print("ALL W2.3 EXTENDED-DESCRIBE WIRE TESTS PASSED")


if __name__ == "__main__":
    main()
