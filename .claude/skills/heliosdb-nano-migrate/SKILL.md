---
name: heliosdb-nano-migrate
description: Migrate an existing app to HeliosDB-Nano with minimal source changes. Covers SQLite (Python `sqlite3` drop-in via the `heliosdb_sqlite` SDK), PostgreSQL (any PG-wire client works unchanged), and MySQL (`--mysql` listener for PHP/WordPress). Documents the dialect-autodetect parser, accepted SQLite-isms (`?` placeholders, `INSERT OR REPLACE/IGNORE`, `INTEGER PRIMARY KEY AUTOINCREMENT`, `sqlite_master`, `PRAGMA *`), and the gotchas that still require code changes. Use this when the user says "port this from sqlite", "switch from sqlite3 to Helios", or "drop-in replace Postgres / MySQL".
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Bash(mysql *), Bash(python3 *), Bash(pip *), Read
---

# Migrating to HeliosDB-Nano

## When to use
- Existing app uses `sqlite3` (Python) and you want to keep the API.
- App uses Postgres and you want to swap PG for Nano.
- App uses MySQL/MariaDB and you want PostgreSQL features without rewriting.

## Compatibility matrix

| Dialect feature | Nano support | Skill section |
|-----------------|--------------|---------------|
| `?` positional placeholders | ✅ (auto-renumber to `$N`) | Recipe 1 |
| `:name` named placeholders | ✅ | Recipe 1 |
| `@name` named placeholders | ✅ | Recipe 1 |
| `INSERT OR REPLACE` / `OR IGNORE` | ✅ (translates to `ON CONFLICT … DO UPDATE/NOTHING`) | Recipe 1 |
| `INTEGER PRIMARY KEY AUTOINCREMENT` | ✅ (translates to `BIGSERIAL`) | Recipe 1 |
| `sqlite_master`, `pg_class`, `information_schema` | ✅ all three coexist | `heliosdb-nano-schema` |
| `PRAGMA foreign_keys / journal_mode / synchronous / busy_timeout` | ✅ no-op-with-ack | Recipe 1 |
| `PRAGMA table_info(t)` | ✅ shaped like SQLite (cid/name/type/notnull/dflt_value/pk) | `heliosdb-nano-schema` |
| `JULIANDAY` / `STRFTIME` | partial | Recipe 1 |
| `DATETIME('now')` | ✅ alias for `CURRENT_TIMESTAMP` | Recipe 1 |
| `\|\|` concat / `IFNULL` / `COALESCE` / `LENGTH` | ✅ | n/a |
| BLOB type | ✅ | `heliosdb-nano-schema` |
| `:memory:` | ✅ via `--memory` or in-memory `connect` | `heliosdb-nano-connect` |
| `lastrowid` | ✅ via Python SDK (transparent `RETURNING`) | Recipe 1 |
| Custom collations | ❌ | n/a |
| Loadable extensions | ❌ | n/a |
| User-defined functions (Python) | ❌ | n/a |
| MySQL `LAST_INSERT_ID()` | ✅ | Recipe 3 |
| MySQL UNIX domain socket | ✅ via `--mysql-socket` | `heliosdb-nano-connect` |

## Recipes

### Recipe 1: Python sqlite3 → HeliosDB-Nano (the primary drop-in)

**Step 1 — install the SDK** (path-dep until on PyPI):
```bash
pip install /path/to/heliosdb-sqlite-3.0.x-py3-none-any.whl
# or, in development:
pip install -e /home/app/Helios/SDKs/sdks/python/
```

**Step 2 — change one import line**:
```python
# before:
import sqlite3

# after (drop-in):
from heliosdb_sqlite import dbapi as sqlite3
```

**Step 3 — change connect target** (your existing `sqlite3.connect("./mydata.db")` already works in embedded mode):
```python
# Embedded — no separate server, one persistent subprocess per Connection
conn = sqlite3.connect("./mydata.db", mode="embedded")

# Daemon — connect to a running heliosdb-nano server (requires psycopg2-binary extra)
# pip install heliosdb-sqlite[daemon]
conn = sqlite3.connect(dsn="postgresql://heliosdb@127.0.0.1:5432/default", mode="daemon")
```

**Step 4 — keep your queries** (both placeholder styles work):
```python
cur = conn.cursor()
cur.execute("INSERT INTO users (email) VALUES (?)", ("a@x.com",))
print(cur.lastrowid)               # transparent RETURNING-clause rewrite

cur.execute("SELECT * FROM users WHERE email = :email",
            {"email": "a@x.com"})
for row in cur.fetchall():
    print(dict(row) if hasattr(row, "keys") else row)
```

**Step 5 — keep your schema** including SQLite-isms:
```sql
-- All of these are accepted as-is by the Nano parser:
CREATE TABLE settings (
    id    INTEGER PRIMARY KEY AUTOINCREMENT,
    key   TEXT UNIQUE,
    value TEXT
);
INSERT OR REPLACE INTO settings (key, value) VALUES ('theme', 'dark');
PRAGMA foreign_keys = ON;             -- no-op (FKs always enforced)
PRAGMA journal_mode = WAL;            -- no-op (WAL is the storage engine)
SELECT name FROM sqlite_master WHERE type='table';
PRAGMA table_info(settings);
```

**Step 6 — switch modes via env (production scale-up)**:
```python
import os
mode = "daemon" if os.environ.get("HELIOSDB_DSN") else "embedded"
conn = sqlite3.connect(
    os.environ.get("HELIOSDB_DSN", "./mydata.db"),
    mode=mode,
)
```

### Recipe 2: PostgreSQL → HeliosDB-Nano
1. Start a Nano server with the same port as your existing Postgres or pick a new one:
```bash
heliosdb-nano start --data-dir ./mydata --port 5432 --listen 127.0.0.1 --auth scram-sha-256 --password 'secret'
```
2. Point your client at it. **No code changes needed** — psycopg2, sqlx, pg8000, JDBC postgres drivers all work unchanged.
3. Run your existing migration scripts (CREATE TABLE / ALTER TABLE / etc.) directly. Most Postgres SQL surfaces work; check `docs/compatibility/plpgsql.md` for PL/pgSQL feature support.

**Schema export from existing Postgres** (one-time):
```bash
pg_dump --schema-only --no-owner mydb > schema.sql
heliosdb-nano repl --data-dir ./mydata < schema.sql
```

### Recipe 3: MySQL/MariaDB → HeliosDB-Nano
1. Start with `--mysql`:
```bash
heliosdb-nano start --data-dir ./mydata \
    --mysql --mysql-listen 127.0.0.1:3306 \
    --mysql-socket /var/run/mysqld/mysqld.sock      # WordPress/PHP local-only
```
2. Point your client at it:
```bash
mysql -h 127.0.0.1 -P 3306 -u heliosdb              # CLI
```
3. WordPress/PHP work unchanged when pointed at the socket. `LAST_INSERT_ID()` is supported.

**Schema export from MySQL**:
```bash
mysqldump --no-data mydb > schema.sql
mysql -h 127.0.0.1 -P 3306 -u heliosdb < schema.sql
```

### Recipe 4: Verify aggregates match (parity test)
```sql
-- Run the same query against the old and new DBs; compare:
SELECT COUNT(*), SUM(total) FROM orders;
```
Acceptance bar: row counts identical, numeric aggregates within ≤ 0.01 % drift (floating-point reorderings are normal across engines).

## Pitfalls
- **Don't mix `?` and `$N` in one statement** — the planner rejects mixed-dialect placeholders.
- **`fastembed_cache/`** is needed only if the importing app also uses `--features code-embed`; otherwise ignore it.
- **`PRAGMA foreign_keys = OFF;` is a no-op-with-ack.** FKs are always enforced. If your old app relied on toggling this off mid-migration, restructure the migration as `BEGIN; … COMMIT;` (see `heliosdb-nano-transactions`).
- **Per-`Connection` subprocess** in Python embedded mode means high request rates create many processes. For threaded servers prefer `mode='daemon'`.
- **Cross-process `INSERT … ON CONFLICT (path) DO UPDATE`** has a known regression (`FEATURE_REQUEST_cross_process_on_conflict.md`). Single-process workflows are unaffected.
- **Custom SQLite functions / collations** registered via `register_function` / `create_function` aren't supported. Reimplement as PL/pgSQL (`heliosdb-nano-schema` Recipe 6) or move that logic into the app.
- **MySQL `ENGINE=InnoDB` and `CHARSET=…` clauses are accepted but ignored.** The storage engine is fixed; charset is UTF-8.

## See also
- `heliosdb-nano-connect` — full mode matrix and TLS/auth wiring.
- `heliosdb-nano-schema` — DDL compatibility and introspection.
- `heliosdb-nano-query` — DML and parameter styles.
- `docs/guides/sqlite_drop_in_tutorial.md` — full tutorial with token-dashboard case study.
- `docs/compatibility/sqlite.md`, `docs/compatibility/plpgsql.md` — feature support matrices.
- `/home/app/Helios/SDKs/sdks/python/heliosdb_sqlite/` — SDK source of truth.
