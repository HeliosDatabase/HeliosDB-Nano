# SQLite drop-in tutorial: porting an app to HeliosDB Nano

## UVP

Take an existing Python app that uses `sqlite3` and retarget it to HeliosDB
Nano with **one import change**. The combined effect of Nano's
SQLite-compat rewrites (engine side) and the
[`heliosdb_sqlite`](https://github.com/Dimensigon/heliosdb-sdks/tree/main/sdks/python)
shim (client side) is true drop-in for the common SQLite surface — `?`
placeholders, `INSERT OR REPLACE/IGNORE`, `INTEGER PRIMARY KEY
AUTOINCREMENT`, `sqlite_master` and `PRAGMA table_info` introspection,
the `Row` factory, `executescript`, etc. No query rewrites required.

## 30-second example

```python
# Before:
# import sqlite3
import heliosdb_sqlite as sqlite3   # one line changed

conn = sqlite3.connect("/tmp/app.db")
conn.row_factory = sqlite3.Row
cur = conn.cursor()

cur.executescript("""
  CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY, mtime REAL, size INTEGER
  );
""")

cur.execute("PRAGMA foreign_keys = ON")    # accepted as a no-op
cur.execute(
    "INSERT OR REPLACE INTO files (path, mtime, size) VALUES (?, ?, ?)",
    ("/tmp/x", 1.0, 100),
)
conn.commit()

cur.execute("SELECT * FROM sqlite_master WHERE type='table'")
print([dict(r) for r in cur.fetchall()])
```

Run it with the Nano binary on `PATH`:

```bash
HELIOSDB_BIN=/path/to/heliosdb-nano python my_app.py
```

## Mode selection

The shim runs in two modes. Pick based on connection lifetime, not
on traffic volume:

| Mode | When to use it | How to pick it |
|------|----------------|----------------|
| **Embedded** | Single-process scripts, CLIs, dev/CI fixtures, anything that opens one connection and holds it. | Default. Just install the SDK and ensure `heliosdb-nano` is on `PATH` (or set `HELIOSDB_BIN=`). |
| **Daemon** | Long-running dashboards, web apps, anything that opens many short-lived connections. | Set `HELIOSDB_DSN=postgresql://user:pw@host:port/db` and pass `mode='daemon'` to `connect()`. |

The PoC port for [token-dashboard](https://github.com/Dimensigon/token-dashboard)
toggles modes from a single env var:

```python
# token_dashboard/db.py
if _USING_HELIOSDB and _HELIOSDB_DSN:
    conn = sqlite3.connect(str(path), mode="daemon", dsn=_HELIOSDB_DSN, timeout=30.0)
else:
    conn = sqlite3.connect(str(path), timeout=30.0)
```

## Step-by-step port (token-dashboard case study)

`token-dashboard` is a Claude-Code analytics dashboard that uses SQLite as
a write-mostly cache: 6 tables, 9 indexes, ~37 `?` placeholders, 6×
`INSERT OR REPLACE`, `sqlite_master` introspection, `PRAGMA
table_info()`, `INTEGER PRIMARY KEY AUTOINCREMENT`, the `Row` factory.

The port reduces to **two file-level changes**, with **zero query
rewrites**.

### 1. Bake the wheel into the project

```bash
mkdir -p wheels
cp /path/to/heliosdb_sqlite-3.0.1-py3-none-any.whl wheels/
```

```diff
 # requirements.txt
 pg8000>=1.30.0
+heliosdb-sqlite @ file:///app/wheels/heliosdb_sqlite-3.0.1-py3-none-any.whl
+psycopg2-binary>=2.9
```

```diff
 # Dockerfile
 FROM python:3.12-slim
 WORKDIR /app

+COPY wheels/ /app/wheels/
 COPY requirements.txt /app/
 RUN pip install --no-cache-dir -r requirements.txt
```

### 2. Backend resolution at the top of `db.py`

```python
import os, re
from contextlib import contextmanager
from pathlib import Path

_BACKEND_PREF = os.environ.get("TOKEN_DASHBOARD_BACKEND", "auto").lower()
_HELIOSDB_DSN = os.environ.get("HELIOSDB_DSN")

if _BACKEND_PREF == "sqlite":
    import sqlite3
    _USING_HELIOSDB = False
elif _BACKEND_PREF == "heliosdb":
    import heliosdb_sqlite as sqlite3
    _USING_HELIOSDB = True
else:                    # "auto" — prefer HeliosDB if installed
    try:
        import heliosdb_sqlite as sqlite3
        _USING_HELIOSDB = True
    except ImportError:
        import sqlite3
        _USING_HELIOSDB = False
```

That's the entire surface of the port. Every existing query, schema, and
PRAGMA call continues to work. Run with `TOKEN_DASHBOARD_BACKEND=sqlite`
to hit stdlib (legacy / rollback path) and unset / `=heliosdb` to hit
HeliosDB.

### 3. Verify

End-to-end smoke test (10 lines, exercises every SQLite-ism the port
relies on):

```python
import os, tempfile
import heliosdb_sqlite as sqlite3

db_path = os.path.join(tempfile.mkdtemp(), "smoke.db")
conn = sqlite3.connect(db_path); conn.row_factory = sqlite3.Row
cur = conn.cursor()
cur.execute("PRAGMA foreign_keys = ON")                                # PRAGMA stub
cur.executescript(                                                    # multi-statement
    "CREATE TABLE files (path TEXT PRIMARY KEY, mtime REAL, size INT);"
    "CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT);"
)
cur.execute("INSERT OR REPLACE INTO files (path, mtime, size) VALUES (?, ?, ?)",
            ("/x", 1.0, 100))                                          # ? + upsert
cur.execute("INSERT OR REPLACE INTO files (path, mtime, size) VALUES (?, ?, ?)",
            ("/x", 2.0, 200))                                          # idempotent
cur.execute("SELECT name FROM sqlite_master WHERE type='table'")       # introspection
assert {"files", "notes"}.issubset({r["name"] for r in cur.fetchall()})
cur.execute("PRAGMA table_info(files)")                                # column shape
assert [r["name"] for r in cur.fetchall()] == ["path", "mtime", "size"]
conn.commit()
print("OK")
```

## Backwards-compat / rollback

If a HeliosDB outage forces a rollback, set `TOKEN_DASHBOARD_BACKEND=sqlite`
and the app reverts to stdlib `sqlite3` at the same `db_path`. The
schema is portable both ways (HeliosDB rewrites the SQLite-isms; stdlib
`sqlite3` runs them natively). Keep this fallback for at least a clean
week in production before removing.

## Limits

- **Embedded mode** spawns a fresh `heliosdb-nano repl` subprocess per
  `Connection`. As of **v3.21** every fresh process rebuilds PK /
  UNIQUE / FK indexes from the on-disk rows on open, so
  cross-connection upserts and uniqueness checks behave correctly.
  Cost is O(rows + indexes) at startup; for very large data dirs,
  switching to **daemon mode** (one long-running server, many
  connections) is still faster — that's what's recommended for
  production.
- **`STRFTIME`, `JULIANDAY`** are intentionally not implemented in the
  engine — they are SQLite-specific names. Use the PG surface (now
  fully audited): `TO_CHAR`, `TO_DATE`, `TO_TIMESTAMP`, `DATE_TRUNC`,
  `DATE_PART`, `EXTRACT`, `AGE`, `MAKE_DATE`, `MAKE_TIMESTAMP`.
- **Python adapters/converters** registered via `register_adapter` /
  `register_converter` aren't bridged into the embedded subprocess.

For the full feature matrix and per-feature source pointers, see
`docs/compatibility/sqlite.md`.
