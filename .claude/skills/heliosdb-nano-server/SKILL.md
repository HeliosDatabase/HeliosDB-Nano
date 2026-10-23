---
name: heliosdb-nano-server
description: Run HeliosDB-Nano as a server (daemon mode) with TLS, authentication, HA replication, and user management. Covers `heliosdb-nano start | stop | status`, TOML configuration sections, the four authentication modes (trust / password / md5 / scram-sha-256), TLS cert/key wiring, the three HA replication roles (standalone / primary / physical standby, one replication port), the three sync modes (async / semi-sync / sync), and per-user account creation. Use this when the user wants a long-running database process, multi-client access over the network, or any HA setup.
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
| HA primary | CLI | `--replication-role primary --replication-port 5433 --replication-token-file ./replication.token --sync-mode semi-sync` |
| HA standby | CLI | `--replication-role standby --primary-host 127.0.0.1:5433 --replication-token-file ./replication.token` (`--primary-host` = the primary's replication port) |
| HA readiness | HTTP / SQL | `curl :8081/ready` (200 ready, 503 not) · `SELECT heliosdb_require_ready()` · primary's `/health` lists `standbys` |
| archived WAL | CLI / SQL | `heliosdb-nano archive list\|verify [--deep]\|gaps\|prune [--dry-run] --data-dir ./mydata` · `SELECT * FROM heliosdb_recovery_window` (also `heliosdb_archived_wal`, `heliosdb_archive_gaps`) · `/health` → `"archive":{…}` |
| recover a standby from segments | CLI | `heliosdb-nano recover --data-dir ./standby --from-archive /srv/segments [--through SEQ] [--then primary --primary-host H:5433 -- <start args>]` (stopped standby; exit 2 = archive gap, range printed) |
| recover a standby from the primary | CLI | `heliosdb-nano recover --data-dir ./standby --from-primary --primary-host H:5433 --replication-token-file ./replication.token [--full\|--no-full-copy] [--full-copy-threshold 50] [--leaf-bytes 4MiB] [--replace] [--stage-dir DIR] [--then primary\|stop]` (stopped standby: nothing / incremental fast-forward / full copy; exit 3 = full copy needed, not applied) |
| archive-only standby | CLI | `--replication-role standby --source archive --archive-source /srv/segments` (no replication connection, no token needed) |
| cascading standby | TOML | `[replication.physical] cascade = true` on a standby + its own `--replication-port`; downstream `--primary-host` = that port |
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

[replication]                # HA replication (docs/guides/physical-standby.md)
role                = "primary"   # standalone | primary | standby
replication_port    = 5433        # THE replication port (NPHYS003); loopback unless tls_enabled
token_file          = "/etc/heliosdb/replication.token"
sync_mode           = "async"     # async | semi-sync | sync (primary: what a commit waits for)
min_acks            = 1           # standbys that must acknowledge (1..=64)
sync_timeout_ms     = 10000
sync_timeout_action = "warn"      # warn | fail
# tls_enabled      = true         # primary: TLS on the replication port (binds --listen's address)
# tls_cert_path    = "/etc/heliosdb/repl.crt"   # else the [server]/--tls-cert pair
# tls_key_path     = "/etc/heliosdb/repl.key"
# tls_ca_path      = "/etc/heliosdb/standby-ca.crt"   # mutual TLS: standbys present a client cert
# snapshot_export_lease_secs = 3600   # an interrupted bootstrap copy resumes within this
# standby only:
# primary_host     = "127.0.0.1:5433"
# primary_tls_ca_path = "/etc/heliosdb/repl-ca.crt"   # TLS is the default for a non-loopback primary
# max_standby_lag  = "30s"
# on_unrecoverable = "stay-up"    # stay-up | exit
# source           = "primary"    # primary | archive (spec P3.2: archive-only standby, --source)
# archive_source   = ["/srv/segments"]   # --archive-source DIR (repeatable), with source = "archive"

# [replication.physical]
# cascade = true                  # STANDBY: serve downstream standbys on its replication_port (P3.2)

[replication.archive]        # archived WAL + recovery window (both roles; spec P3.1)
# enabled             = true            # --archive=false
# dir                 = "/srv/nano/primary.archive"   # default <data dir>.archive; never inside the data dir
# segment_max_bytes   = "64MiB"         # --archive-segment-max-bytes
# segment_max_secs    = 300
# fsync_bytes         = "8MiB"
# recovery_window     = "7d"            # --archive-recovery-window
# max_bytes           = "unlimited"     # pruning inside the window SHORTENS it (WARN), never stalls writes
# disk_reserve        = "10%"           # or a size, e.g. "20GiB"
# prune_interval_secs = 60
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

### Recipe 4: HA Tier-1 — primary + physical standby
A standby is a read-only **physical replica**: it applies the primary's RocksDB write groups over ONE replication connection to the primary's `--replication-port`. Full guide: `docs/guides/physical-standby.md`. On one host the connection is plaintext on loopback (below); a standby on **another host** uses TLS (Recipe 4b). An interrupted bootstrap copy resumes where it stopped (the primary leases the snapshot for `snapshot_export_lease_secs`, default 1 h).
```bash
install -d -m 700 /srv/nano                       # owner-only parent of both data dirs
head -c 32 /dev/urandom > /srv/nano/replication.token && chmod 600 /srv/nano/replication.token
```
**Primary:**
```bash
heliosdb-nano start \
    --data-dir /srv/nano/primary --port 5432 --http-port 8080 \
    --replication-role primary \
    --replication-port 5433 \
    --replication-token-file /srv/nano/replication.token \
    --sync-mode semi-sync \
    --daemon --pid-file /srv/nano/primary.pid
```
**Standby** (absent or empty data dir → bootstraps a verified copy, then follows; restart → resumes at its cursor):
```bash
heliosdb-nano start \
    --data-dir /srv/nano/standby --port 5442 --http-port 8081 \
    --replication-role standby \
    --primary-host 127.0.0.1:5433 \
    --replication-token-file /srv/nano/replication.token \
    --daemon --pid-file /srv/nano/standby.pid
```
Verify:
```bash
curl -s http://127.0.0.1:8081/ready      # 200 when streaming; body = state, cursor, lag
curl -s http://127.0.0.1:8080/health     # primary: "standbys":[{…,"applied":"0/1F4",…}]
psql -h 127.0.0.1 -p 5442 -c "SELECT heliosdb_require_ready(), pg_is_in_recovery()"
```
Sync tuning lives in `[replication]`: `min_acks`, `sync_timeout_ms`, `sync_timeout_action = "warn" | "fail"` (`fail` errors a commit that IS durable on the primary — never blind-retry it).
**No failover or promotion yet** (plan phase P5): `SWITCHOVER` is refused and a standby's data directory cannot be reopened as a primary. A standby in `needs_recovery` (history mismatch, pruned WAL, wrong token, protocol version) stays up refusing data reads with SQLSTATE 55000. A pruned WAL is first filled from the primary's WAL archive automatically (Recipe 4d); only a gap neither holds is refused (the reason names the range and both floors). Recover it from copied segments (`heliosdb-nano recover --from-archive`, Recipe 4d), or from the primary (`heliosdb-nano recover --from-primary`, Recipe 4e: ships only the key ranges that differ, or a full copy swapped in with `--replace`). A TLS failure (wrong CA, bad certificate) is NOT `needs_recovery`: the standby keeps reconnecting with `"reason": "tls: …"` on `/health`. Loopback test pair: `scripts/regress/nano-physical-pair.sh --binary <path> --start` (add `--tls` for a TLS pair).

### Recipe 4b: Remote standby over TLS
```bash
# Primary (10.0.0.5): --replication-tls binds the replication port on --listen's address.
heliosdb-nano start --data-dir /srv/nano/primary --listen 10.0.0.5 --port 5432 \
    --replication-role primary --replication-port 5433 \
    --replication-token-file /srv/nano/replication.token \
    --replication-tls --replication-tls-cert /etc/heliosdb/repl.crt --replication-tls-key /etc/heliosdb/repl.key
#   add --replication-tls-ca /etc/heliosdb/standby-ca.crt for mutual TLS
# Standby (another host): TLS is the default for a non-loopback --primary-host.
heliosdb-nano start --data-dir /srv/nano/standby --listen 10.0.0.6 --port 5432 \
    --replication-role standby --primary-host 10.0.0.5:5433 \
    --primary-ca /etc/heliosdb/repl-ca.crt \
    --replication-token-file /srv/nano/replication.token
#   mutual TLS: --primary-client-cert standby.crt --primary-client-key standby.key
curl -s http://127.0.0.1:8080/health | grep '"tls":true'
```
The token is still required over TLS. The certificate must name the `--primary-host` host (an IP is checked as an IP SAN; `--primary-server-name` overrides). Plaintext over a network needs `--allow-insecure-replication-remote` on both sides (dev only, INSECURE warning). A standby re-reads its CA/client cert at every reconnect; the primary loads its cert at start. Give a standby the primary's encryption key; there is no key rotation command, and rotation with standbys attached is unsupported until P5.

### Recipe 4c: Archived WAL and the recovery window
Every primary serving its replication port and every standby seals each write group it applies into `<data dir>.archive/<history>/<epoch>/<first>-<last>.hseg` (Oracle: archived redo logs; on by default). Segments are kept for `--archive-recovery-window` (default `7d`), bounded by `--archive-max-bytes` and `--archive-disk-reserve` (default `10%`). A full archive **never stalls the primary**: pruning inside the window shortens it, with one WARN (`recovery window shortened to …`) and `shortened = true`. Recovery FROM the archive: Recipe 4d.
```bash
psql -h 127.0.0.1 -p 5432 -c "SELECT configured_window_secs, effective_window_secs, segments, bytes_retained, shortened, state FROM heliosdb_recovery_window"
heliosdb-nano archive list   --data-dir /srv/nano/primary            # one line per segment (available / corrupt)
heliosdb-nano archive verify --data-dir /srv/nano/primary --deep     # re-hash + decode every frame; --key-file for an encrypted dataset
heliosdb-nano archive gaps   --data-dir /srv/nano/primary
heliosdb-nano archive prune  --data-dir /srv/nano/primary --dry-run  # a real prune is refused while the node runs
```
Each node needs its own archive directory (the archiver locks it). Moving a standby's data directory aside for a rebuild? Move its `<data dir>.archive` with it.

### Recipe 4d: Recovering a standby from archived WAL (spec P3.2)
- **Gap fill (automatic, Oracle FAL).** A standby away longer than the primary's RocksDB WAL retention is served the missing write groups from the primary's archive on its replication connection; the primary logs `serving a receiver … from the WAL archive`, its `/health` `standbys[]` entry shows `"source":"archive"`, then `"wal"`. Only a gap the archive cannot cover is refused `WalPruned` (range + both floors named).
- **`recover` (Oracle `RECOVER STANDBY DATABASE`).** On a STOPPED standby (a running one is refused, naming its pid), from segments copied into any directories (any node of the same history; copies dedupe by SHA-256). Every segment is verified deep BEFORE anything is applied; a corrupt or foreign one is named, exit 1, nothing applied. Exit 2 + `needs recovery: archive gap: sequences S..T` when the segments stop short. Re-run = no-op. An empty data dir is refused: a standby starts from a snapshot of the primary.
```bash
heliosdb-nano stop --pid-file /srv/nano/standby.pid
heliosdb-nano recover --data-dir /srv/nano/standby --from-archive /srv/copied-segments --config standby.toml
heliosdb-nano recover --data-dir /srv/nano/standby --from-archive /srv/copied-segments \
    --then primary --primary-host 127.0.0.1:5433 -- --port 5442 --replication-token-file /srv/nano/replication.token
```
- **Archive-only standby (managed recovery from archived logs).** `--source archive --archive-source DIR` (repeatable): no replication connection, posture `recovering` (mode `archive`, progress `"<cursor> of <newest>"`), ready while the segment holding its cursor is ≤ `max_standby_lag` old (else `catching_up`); a missing segment → `needs_recovery` naming the range, resumes when it appears. `heliosdb_standby_status.primary` = `archive:<dir>`. The data dir must already be an installed standby.
- **Cascading standby.** `[replication.physical] cascade = true` + `--replication-port 5443` on standby A; standby B uses `--primary-host 127.0.0.1:5443`. A strips its node-local records and serves the primary's groups/sequence numbers; A's `/health` has `"cascade":true` and lists B.

### Recipe 4e: Recovering a standby from the primary (spec P4.1)
Oracle `RECOVER STANDBY DATABASE FROM SERVICE`. On a STOPPED standby (a running one is refused, naming its pid), over the primary's replication port with the standby's token and TLS flags (as `resync`). The ladder:
1. **Nothing to copy** — the primary still serves the next write group (WAL or archive): `recover: the primary still serves sequence N…: nothing to copy`.
2. **Incremental fast-forward** — refused `WalPruned`/`Gap`: both sides digest the keyspace in `--leaf-bytes` leaves (default 4 MiB, SHA-256), only differing leaves travel as SST files (range-delete + ingest, 64 per batch), then the stage is re-cut at the primary's cut `T` (`start` resumes at `T`). Raw bytes: an encrypted dataset needs no key for this.
3. **Full copy** — another history/epoch (`HistoryMismatch`), `--full`, or differing bytes > `--full-copy-threshold` % (default 50) of the primary's: into an absent/empty dir directly; else into `<dir>.recover-full`, swapped in only with `--replace` (old dir kept as `<dir>.replaced-<UTC>`), otherwise exit 3 printing the two `mv`s (a re-run with `--replace` reuses the copy). `--no-full-copy` → exit 3 with the estimate.
```bash
heliosdb-nano stop --pid-file /srv/nano/standby.pid
heliosdb-nano recover --data-dir /srv/nano/standby --from-primary --primary-host 10.0.0.5:5433 \
    --replication-token-file /srv/nano/replication.token --primary-ca /etc/heliosdb/repl-ca.crt --then stop
heliosdb-nano recover --data-dir /srv/nano/standby --from-primary --primary-host 10.0.0.5:5433 \
    --replication-token-file /srv/nano/replication.token --replace   # allow a full copy to replace the dir
```
A fast-forward killed part-way leaves a record: `start` stays up in `needs_recovery` (`/ready` 503) with `fast-forward from the primary interrupted at sequence S→T: run heliosdb-nano recover --from-primary … to finish it`; re-run the same command (it continues the same cut while the primary's export lease holds). Stage dir `--stage-dir` (default `<dir>.recover-stage`, never inside the data dir) needs `2 × leaf × 64` free. Exit codes: 0 done · 1 refused/failed · 2 (`--from-archive` gap / usage) · 3 full copy needed, not applied. The standby's own archive lacks `S+1..T` afterwards; a downstream of a fast-forwarded cascading standby is recovered the same way against it.

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
- **Standbys never forward writes.** Writes, DDL and `nextval()` on a standby fail with SQLSTATE 25006; connect to the primary to write. A standby refuses `--mysql`/`--mysql-socket`, and its HTTP port serves only `/`, `/health`, `/ready`, `/version`.
- **`--primary-host` is the primary's replication port**, never its PostgreSQL or HTTP port (the standby says so if you get it wrong). Both nodes need the same 32-byte, mode-0600 `--replication-token-file`; a primary without one (or in memory) serves no replication port, and then no commit ever waits for a standby.
- **A 4.41.x logical standby directory is refused** ("rebuilt once: start with a new, empty data directory"). There is no in-place conversion.
- **Don't expose `0.0.0.0:5432` without TLS+auth**. Trust auth on a public bind is a wide-open database.
- **Daemon mode keeps a PID file**; if the file disappears (e.g., `/var/run` cleared on reboot) `stop` and `status` cannot find the process. Use `pkill -f heliosdb-nano` as a fallback and inspect the data dir for stale lock files.

## See also
- `heliosdb-nano-deploy` — Docker/compose, Fly.io, Railway, Render specifics.
- `heliosdb-nano-observability` — `/health`, slow-query log, `\stats`, tracing.
- `heliosdb-nano-backup` — combine `--dump-schedule` with daemon mode.
- `docs/guides/physical-standby.md` — physical standby: start, readiness, Proxy contract, sync modes, archived WAL and the recovery window, rebuild, limits.
- `docs/guides/ha_testing.md` — HA test suites and the pair fixture (the `tests/docker` lab is still out of service).
- `docs/guides/audit.md` — audit-log configuration.
- `config.example.toml` — full TOML config schema.
