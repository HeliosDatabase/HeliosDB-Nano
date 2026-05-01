---
name: heliosdb-nano-backup
description: Backup and restore HeliosDB-Nano. Two surfaces — CLI (`heliosdb-nano dump` / `restore`) and library (`db.dump_full` / `db.restore_from_dump` / `db.restore_tables`). Compression options (zstd default, gzip, brotli, none). Append (incremental) backups. Periodic background dumps via `--dump-schedule "0 */6 * * *"`. Verification on restore. Use this for routine backups, partial table restores, in-memory persistence, and migrating a data dir between hosts.
allowed-tools: Bash(heliosdb-nano *), Read
---

# Backup & Restore

## When to use
- Daily / hourly backups.
- Migrating a data directory to another host.
- Persisting an `--memory` server before shutdown.
- Restoring a single table without rolling back the whole DB.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| full dump | CLI | `heliosdb-nano dump --output bk.heliodump --data-dir ./mydata` |
| compressed dump | CLI | `heliosdb-nano dump --output bk.heliodump.zst --data-dir ./mydata --compression zstd` |
| append (incremental) | CLI | `heliosdb-nano dump --output bk.heliodump --data-dir ./mydata --append` |
| dump from server | CLI | `heliosdb-nano dump --output bk.heliodump --connection postgresql://heliosdb@127.0.0.1:5432` |
| scheduled dump | CLI (start) | `heliosdb-nano start --data-dir ./mydata --dump-schedule "0 */6 * * *"` |
| dump on shutdown | CLI (start/repl) | `heliosdb-nano repl --memory --dump-on-shutdown --dump-file bk.heliodump` |
| restore (full) | CLI | `heliosdb-nano restore --input bk.heliodump --target ./newdata` |
| restore (verify first) | CLI | `heliosdb-nano restore --input bk.heliodump --target ./newdata --verify` |
| restore (server) | CLI | `heliosdb-nano restore --input bk.heliodump --connection postgresql://heliosdb@…` |
| library: full dump | Rust | `db.dump_full("./bk.heliodump")?` |
| library: restore | Rust | `db.restore_from_dump("./bk.heliodump")?` |
| library: partial restore | Rust | `db.restore_tables("./bk.heliodump", &["users", "orders"])?` |
| REPL: SQL-export | REPL | `\dump dump.sql` (text-level export, not the binary format) |

## Recipes

### Recipe 1: Daily backup with rotation
```bash
#!/usr/bin/env bash
set -euo pipefail
DATA_DIR=./mydata
BK_DIR=./backups
mkdir -p "$BK_DIR"
ts=$(date +%Y%m%d-%H%M%S)
heliosdb-nano dump \
    --output "$BK_DIR/heliodump-$ts.zst" \
    --data-dir "$DATA_DIR" \
    --compression zstd
# keep the last 14 daily backups
ls -t "$BK_DIR"/heliodump-*.zst | tail -n +15 | xargs -r rm
```

### Recipe 2: Restore to a fresh data dir + verify
```bash
heliosdb-nano restore \
    --input  ./backups/heliodump-20260429-090000.zst \
    --target ./restored-data \
    --verify

heliosdb-nano repl --data-dir ./restored-data       # sanity-check
```
`--verify` validates checksums and metadata before applying — recommended for any restore that overwrites a non-empty target.

### Recipe 3: Partial restore (one table only)
```rust
let db = EmbeddedDatabase::new("./mydata")?;
db.restore_tables("./bk.heliodump", &["orders".to_string()])?;
```
Useful after a localized data-loss event (e.g., one table truncated by mistake): you keep all your other tables as-is.

### Recipe 4: Periodic dump on a running server
```bash
heliosdb-nano start \
    --data-dir ./mydata \
    --dump-schedule "0 */6 * * *" \
    --daemon
# every 6 hours, a backup is written next to ./mydata
```

### Recipe 5: Persisting an in-memory database
```bash
heliosdb-nano repl --memory \
    --dump-on-shutdown \
    --dump-file ./mem-snapshot.heliodump
# Ctrl-D / \q triggers a final dump on the way out.
```

### Recipe 6: Append (incremental) backups
```bash
# week 0 — full
heliosdb-nano dump --output weekly.heliodump --data-dir ./mydata
# week 1 — append since last
heliosdb-nano dump --output weekly.heliodump --data-dir ./mydata --append
# week 2 — append again
heliosdb-nano dump --output weekly.heliodump --data-dir ./mydata --append
```
Restore folds all appended segments together — the file remains one logical archive.

### Recipe 7: Backup against a running server (no filesystem access)
```bash
heliosdb-nano dump \
    --output bk.heliodump \
    --connection postgresql://heliosdb@db.example.com:5432/default
```
The server streams its on-disk state through the wire — no need for SSH access to the data dir.

## Pitfalls
- **Default output filename is `backup.heliodump`** if `--output` is omitted. Easy to overwrite by accident — always pass `--output`.
- **`zstd` is the default compression** and almost always the right choice. `gzip` for legacy tools; `brotli` for absolute size; `none` for already-compressed media.
- **`restore` to a non-empty `--target` overwrites it**. Use `--verify` first; consider backing up the target before restore.
- **`\dump` (REPL) writes SQL text, not the binary `.heliodump`.** Useful for inspection / cross-DB import; not the right format for full-fidelity restores (no MVCC history, no branches).
- **Appended dumps must restore in order**. Storing each segment as a separate file is fine; the CLI handles concatenation logically.
- **Branches and time-travel snapshots ARE preserved** in `.heliodump`. The full archive is a faithful snapshot of the database, not just the live tables.

## See also
- `heliosdb-nano-server` — `--dump-schedule`, `--dump-on-shutdown` flags.
- `heliosdb-nano-time-travel` — short-window history; `dump` is the durable counterpart.
- `heliosdb-nano-deploy` — backup volume mounts in container deployments.
