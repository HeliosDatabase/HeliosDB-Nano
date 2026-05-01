---
name: heliosdb-nano-connect
description: Open a connection to HeliosDB-Nano. Covers all five connection modes — embedded library (Rust), PostgreSQL wire (psql / psycopg2 / sqlx / pg8000), MySQL wire (mysql / WordPress / PHP), REPL, and the Python sqlite3 drop-in SDK. Pick the right one for the workload, set auth and TLS, and verify the connection. Use this when the user says "connect to", "psql", "open a session", or wants to talk to a Nano server already running on a host.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Bash(mysql *), Bash(python3 *), Read
---

# Connect to HeliosDB-Nano

## When to use
- A binary is installed and you need to open a session.
- You need to know which client library / wire protocol fits a given language.
- You need TLS, password auth, or socket-based connections.

## Connection mode matrix

| Mode | Latency | Concurrency | Best for | Skill section |
|------|---------|-------------|----------|---------------|
| Embedded (Rust library) | sub-ms | single-process | Rust apps, CLIs, embedded analytics | Recipe 1 |
| REPL | n/a (interactive) | one human | Exploration, ops, schema work | Recipe 2 |
| PG wire (TCP) | ~0.5–2 ms | many | Postgres-flavored apps in any language | Recipe 3 |
| PG wire (Unix socket) | ~0.2–1 ms | many | Local-only, lower latency, no TCP overhead | Recipe 3b |
| MySQL wire | ~0.5–2 ms | many | PHP/WordPress/MySQL clients (`--mysql` flag required) | Recipe 4 |
| Python `sqlite3` drop-in | varies by mode | depends | Apps that already use `import sqlite3` | Recipe 5 |
| MCP (stdio/HTTP/WS) | varies | one agent | AI agents (Claude Code, Codex CLI, MCP-aware tools) | `heliosdb-nano-mcp` |

## Recipes

### Recipe 1: Embedded (Rust)
```rust
use heliosdb_nano::{EmbeddedDatabase, Result};

fn main() -> Result<()> {
    // File-backed
    let db = EmbeddedDatabase::new("./mydata")?;

    // Or in-memory (test fixtures, ephemeral workloads)
    // let db = EmbeddedDatabase::new_in_memory()?;

    // Or with a custom config
    // let db = EmbeddedDatabase::with_config(my_config)?;

    db.execute("CREATE TABLE IF NOT EXISTS t (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO t (id, name) VALUES (1, 'alice')")?;
    let rows = db.query("SELECT id, name FROM t WHERE id = $1", &[&1])?;
    println!("{} rows", rows.len());
    Ok(())
}
```
`Cargo.toml`:
```toml
[dependencies]
heliosdb-nano = "3.22"
```
For features (`code-graph`, `mcp-endpoint`, etc.), add `features = ["..."]`.

### Recipe 2: REPL (interactive)
```bash
heliosdb-nano repl --data-dir ./mydata          # file-backed
# or:
heliosdb-nano repl --memory                      # ephemeral
```
Inside the REPL:
```
heliosdb> \d              -- list tables
heliosdb> \timing          -- toggle timing
heliosdb> SELECT 1+1;
heliosdb> \q
```
Full meta-command reference: `\help` inside the REPL.

### Recipe 3: PG wire (TCP)
**Server** (terminal A):
```bash
heliosdb-nano start --data-dir ./mydata --port 5432 --listen 127.0.0.1 --auth trust
```
**Client** (terminal B — psql):
```bash
psql -h 127.0.0.1 -p 5432 -U heliosdb -d default
```
**Client (Python — psycopg2)**:
```python
import psycopg2
conn = psycopg2.connect(host="127.0.0.1", port=5432, user="heliosdb", dbname="default")
cur = conn.cursor()
cur.execute("SELECT 1")
print(cur.fetchone())
```
**Auth modes** (`--auth`): `trust` | `password` | `md5` | `scram-sha-256`. Use `--password <STR>` to seed an admin password for non-trust modes.

### Recipe 3b: PG wire over Unix socket (local-only, low overhead)
**Server**:
```bash
heliosdb-nano start --data-dir ./mydata --pg-socket-dir /tmp
# listens at /tmp/.s.PGSQL.5432 — libpq's default convention
```
**Client**:
```bash
psql -h /tmp -p 5432 -U heliosdb       # -h <dir> = unix socket
```

### Recipe 4: MySQL wire
**Server**:
```bash
heliosdb-nano start --data-dir ./mydata --mysql --mysql-listen 127.0.0.1:3306
# Optional Unix socket for PHP/WordPress:
# heliosdb-nano start --data-dir ./mydata --mysql --mysql-socket /tmp/heliosdb-mysql.sock
```
**Client**:
```bash
mysql -h 127.0.0.1 -P 3306 -u heliosdb
```
**WordPress / PHP**:
```php
$conn = mysqli_connect('localhost', 'heliosdb', '', 'wordpress');
// Or via socket:
$conn = mysqli_connect(null, 'heliosdb', '', 'wordpress', null, '/tmp/heliosdb-mysql.sock');
```

### Recipe 5: Python `sqlite3` drop-in
The `heliosdb_sqlite` SDK is a 100% sqlite3-API-compatible wrapper. Two run-modes:
- **`mode='embedded'`** (default) — spawns a persistent `heliosdb-nano repl` subprocess. Zero-config; one process per Connection.
- **`mode='daemon'`** — connects to a running Nano server over PG wire (requires `psycopg2-binary` extra).

```python
# pip install heliosdb-sqlite[daemon]
from heliosdb_sqlite import connect

# Embedded (file-backed)
conn = connect('./mydata.db', mode='embedded')

# Daemon (server already running on localhost:5432)
# conn = connect(dsn='postgresql://heliosdb@127.0.0.1:5432/default', mode='daemon')

cur = conn.cursor()
cur.execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)")
cur.execute("INSERT INTO users (name) VALUES (?)", ("alice",))
print(cur.lastrowid)              # transparent RETURNING-clause rewrite
for row in cur.execute("SELECT id, name FROM users"):
    print(dict(row) if hasattr(row, 'keys') else row)
conn.close()
```
Apps that import `sqlite3` can switch with one line:
```python
from heliosdb_sqlite import dbapi as sqlite3
```
See `heliosdb-nano-migrate` for full sqlite3 → Nano porting checklist.

### Recipe 6: TLS (encrypt-in-transit)
**Server**:
```bash
heliosdb-nano start --data-dir ./mydata \
  --tls-cert ./certs/server.crt \
  --tls-key  ./certs/server.key \
  --auth scram-sha-256 --password 'changeme'
```
**Client (psql)**:
```bash
PGSSLMODE=require psql -h db.example.com -p 5432 -U heliosdb
```

## Pitfalls
- **`--auth password|md5|scram-sha-256` requires `--password`** at startup. Without it the server refuses to start.
- **`--mysql` flag is required** for MySQL-wire connections to work; the listener is off by default.
- **Embedded mode (Rust) is single-process**. Multiple OS processes opening the same data directory will see locking errors. For multi-process, run `start` and connect via PG wire.
- **`heliosdb_sqlite` `mode='embedded'` spawns a subprocess per `Connection`**. For high-concurrency Python apps prefer `mode='daemon'` against a running server.
- **Default port 5432 collides with PostgreSQL**. If both are on the host, use `--port 5433` or stop PostgreSQL.

## See also
- `heliosdb-nano-server` — daemon mode, auth, TLS, HA replication flags.
- `heliosdb-nano-mcp` — connect from MCP-aware AI agents.
- `heliosdb-nano-migrate` — port a sqlite3-based app.
- `src/main.rs` — exact CLI flag definitions.
