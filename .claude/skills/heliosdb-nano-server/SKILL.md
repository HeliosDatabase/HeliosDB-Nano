---
name: heliosdb-nano-server
description: Run HeliosDB-Nano as a server (daemon mode) with TLS, authentication, HA replication, and user management. Covers `heliosdb-nano start | stop | status`, TOML configuration sections, the four authentication modes (trust / password / md5 / scram-sha-256), TLS cert/key wiring, the four HA replication roles (standalone / primary / standby / observer), the three sync modes (async / semi-sync / sync), and per-user account creation. Use this when the user wants a long-running database process, multi-client access over the network, or any HA setup.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Bash(systemctl *), Read
---

# Server Operations (Daemon Mode, TLS, Auth, HA, Users)

## When to use
- Multiple processes / hosts need to share one Nano database.
- You need encryption-in-transit (TLS).
- You need password / md5 / SCRAM-SHA-256 authentication.
- You need a warm standby (tier 1), active-active (tier 2), or sharded (tier 3) topology.

> **Risk note**: this skill is production-affecting. Confirm with the user before running `start --daemon`, `stop`, force-kill, or HA reconfiguration in production.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| start (foreground) | CLI | `heliosdb-nano start --data-dir ./mydata` |
| start (daemon) | CLI | `heliosdb-nano start --data-dir ./mydata --daemon --pid-file ./heliosdb.pid` |
| stop | CLI | `heliosdb-nano stop --pid-file ./heliosdb.pid` |
| status | CLI | `heliosdb-nano status --pid-file ./heliosdb.pid` |
| TLS | CLI | `--tls-cert ./certs/srv.crt --tls-key ./certs/srv.key` |
| auth | CLI | `--auth scram-sha-256 --password 'admin-secret'` |
| HA primary | CLI | `--replication-role primary --standby-hosts 'host1:5433,host2:5433' --sync-mode semi-sync` |
| HA standby | CLI | `--replication-role standby --primary-host primary:5433` |
| HA observer | CLI | `--replication-role observer --observer-hosts '…'` |
| MySQL listener | CLI | `--mysql --mysql-listen 127.0.0.1:3306` |
| Unix socket (PG) | CLI | `--pg-socket-dir /tmp` |
| Unix socket (MySQL) | CLI | `--mysql-socket /tmp/heliosdb-mysql.sock` |
| HTTP health port | CLI | `--http-port 8080` |
| TOML config | CLI | `--config ./config.toml` |
| user management | REPL | `\user list`, `\user add <n>`, `\user remove <n>`, `\password <n>` |
| server status (REPL) | REPL | `\server status` / `\server start` / `\server stop` |
| TLS status | REPL | `\ssl status` |
| reload config | REPL | `\config reload` |

## TOML configuration sections

`config.example.toml` (in repo root) is the template. Key sections used in this skill:

```toml
[server]
listen_addr     = "127.0.0.1"
port            = 5432
max_connections = 200

[storage]
path                  = "./mydata"
memory_only           = false
wal_enabled           = true
compression           = "zstd"

[encryption]
key_source = "file"          # or "env"
cipher     = "aes-256-gcm"
zke_mode   = false           # zero-knowledge encryption

[performance]
cache_size_mb     = 512
wal_buffer_size   = 65536

[session]
timeout_secs            = 3600
max_sessions_per_user   = 50

[locks]
deadlock_detection_enabled = true
timeout_ms                 = 5000

[audit]
enabled    = true
log_path   = "./logs/audit.jsonl"

[sync]                       # HA replication knobs
heartbeat_secs = 5
fail_after     = 3
```

## Recipes

### Recipe 1: Foreground server, trust auth (development)
```bash
heliosdb-nano start --data-dir ./mydata --port 5432 --listen 127.0.0.1
# Logs stream to stdout. Ctrl-C stops.
```

### Recipe 2: Daemon mode with PID file
```bash
heliosdb-nano start \
    --data-dir ./mydata \
    --daemon \
    --pid-file /var/run/heliosdb.pid

heliosdb-nano status --pid-file /var/run/heliosdb.pid
heliosdb-nano stop   --pid-file /var/run/heliosdb.pid
```

### Recipe 3: TLS + SCRAM-SHA-256 (production-ish)
```bash
heliosdb-nano start \
    --data-dir ./mydata \
    --tls-cert ./certs/server.crt \
    --tls-key  ./certs/server.key \
    --auth scram-sha-256 \
    --password 'admin-strong-secret' \
    --daemon --pid-file ./heliosdb.pid
```
Client:
```bash
PGSSLMODE=require psql -h db.example.com -p 5432 -U heliosdb
```

### Recipe 4: HA Tier-1 — primary + warm standby
**Primary (host A):**
```bash
heliosdb-nano start \
    --data-dir ./data-primary \
    --replication-role primary \
    --replication-port 5433 \
    --standby-hosts 'hostB:5433' \
    --sync-mode semi-sync \
    --daemon --pid-file ./primary.pid
```
**Standby (host B):**
```bash
heliosdb-nano start \
    --data-dir ./data-standby \
    --replication-role standby \
    --primary-host hostA:5433 \
    --daemon --pid-file ./standby.pid
```
Verify via REPL on either node:
```
heliosdb> \replication
```
Failover: stop the primary, promote the standby (`--replication-role primary`), restart.

### Recipe 5: Active-active (Tier 2, requires `--features ha-tier2`)
```bash
# Each node runs as both primary and reachable from peers.
heliosdb-nano start \
    --data-dir ./node-a \
    --replication-role primary \
    --node-id 11111111-1111-1111-1111-111111111111 \
    --standby-hosts 'nodeB:5433,nodeC:5433' \
    --sync-mode async
```
Conflict resolution uses vector clocks at branch level — see `docs/guides/ha_cluster_tutorial.md`.

### Recipe 6: User management (REPL)
```
heliosdb> \user list
heliosdb> \user add reportreader
heliosdb> \password reportreader
   New password: ********
   Re-type:      ********
heliosdb> \user remove olduser
```
Or via SQL:
```sql
CREATE USER reportreader WITH PASSWORD 'changeme';
ALTER USER reportreader WITH PASSWORD 'newsecret';
DROP USER olduser;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO reportreader;
```

### Recipe 7: systemd unit (template — not pre-baked)
```ini
# /etc/systemd/system/heliosdb-nano.service
[Unit]
Description=HeliosDB-Nano
After=network.target

[Service]
User=heliosdb
ExecStart=/usr/local/bin/heliosdb-nano start \
    --data-dir /var/lib/heliosdb \
    --port 5432 \
    --listen 0.0.0.0 \
    --auth scram-sha-256 --password ${HELIOSDB_ADMIN_PASSWORD} \
    --daemon --pid-file /var/run/heliosdb.pid
ExecStop=/usr/local/bin/heliosdb-nano stop --pid-file /var/run/heliosdb.pid
PIDFile=/var/run/heliosdb.pid
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

## Pitfalls
- **`--data-dir` and `--memory` are mutually exclusive**, but exactly one is required for `start`. Forgetting both is a startup error.
- **TLS requires *both* `--tls-cert` and `--tls-key`** — passing one without the other refuses to start.
- **`--auth password|md5|scram-sha-256` requires `--password`** for the seed admin account.
- **`--port` defaults to 5432**, which collides with PostgreSQL. On a host running both, set `--port 5433` (or stop PG).
- **Standby write-forwarding** uses env var `HELIOSDB_PRIMARY_PG_PORT` (default 5432) to know which primary port to forward to.
- **Don't expose `0.0.0.0:5432` without TLS+auth**. Trust auth on a public bind is a wide-open database.
- **Daemon mode keeps a PID file**; if the file disappears (e.g., `/var/run` cleared on reboot) `stop` and `status` cannot find the process. Use `pkill -f heliosdb-nano` as a fallback and inspect the data dir for stale lock files.

## See also
- `heliosdb-nano-deploy` — Docker/compose, Fly.io, Railway, Render specifics.
- `heliosdb-nano-observability` — `/health`, slow-query log, `\stats`, tracing.
- `heliosdb-nano-backup` — combine `--dump-schedule` with daemon mode.
- `docs/guides/ha_cluster_tutorial.md` — full HA topology guide.
- `docs/guides/audit.md` — audit-log configuration.
- `config.example.toml` — full TOML config schema.
