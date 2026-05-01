---
name: heliosdb-nano-observability
description: Tracing, slow-query logging, health checks, statistics, and metrics in HeliosDB-Nano. Covers `RUST_LOG` filters, the WARN-level slow-query log (1 s default threshold), the `GET /health` HTTP endpoint, REPL diagnostic commands (`\stats`, `\compression`, `\optimize`, `\indexes`), and Prometheus integration where wired. Use this when the user is debugging performance, asking "why is this slow", monitoring uptime, or wiring Nano into Grafana / their alerting stack.
allowed-tools: Bash(heliosdb-nano *), Bash(curl *), Bash(psql *), Read
---

# Observability — Tracing, Metrics, Diagnostics

## When to use
- Debug a slow query.
- Wire Nano into a monitoring stack.
- Check the database is healthy from the outside.
- Get index / compression / vacuum recommendations.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| set log level | env | `RUST_LOG=info,heliosdb=debug heliosdb-nano start …` |
| trace one module | env | `RUST_LOG=heliosdb_nano::sql::planner=trace …` |
| /health | HTTP | `curl http://127.0.0.1:8080/health` |
| change health port | CLI | `--http-port 9090` |
| slow-query log | (auto) | logged at `WARN` for any statement > 1 s |
| stats (REPL) | REPL | `\stats` |
| compression stats | REPL | `\compression [<table>]` |
| optimize hints | REPL | `\optimize <table>` |
| index recommendations | REPL | `\indexes <table>` |
| toggle query timing | REPL | `\timing` |
| EXPLAIN | SQL | `EXPLAIN [ANALYZE] SELECT …` |

## Recipes

### Recipe 1: Verbose tracing for a single startup
```bash
RUST_LOG=info,heliosdb_nano=debug heliosdb-nano start --data-dir ./mydata
# In another terminal:
heliosdb-nano repl --data-dir ./mydata
heliosdb> SELECT * FROM users;          # tracing logs scroll in the server terminal
```

### Recipe 2: Filter to one module (rare deep-dive)
```bash
RUST_LOG=warn,heliosdb_nano::sql::planner=trace heliosdb-nano start …
```

### Recipe 3: HTTP health probe
```bash
heliosdb-nano start --data-dir ./mydata --http-port 8080 --daemon --pid-file h.pid &
curl -s http://127.0.0.1:8080/health
# {"status":"ok"}
```
Use this for k8s liveness/readiness probes, Fly.io health checks, etc.

### Recipe 4: Catch slow queries
The slow-query log fires at `WARN` for any single statement that exceeds 1 second. Run the server with `RUST_LOG=warn` (default) and they show up immediately:
```
WARN slow query (2.341 s): SELECT * FROM big_table WHERE …
```
Threshold is currently fixed at 1 s; treat anything that fires as a candidate for an index or query rewrite.

### Recipe 5: Index recommendations (REPL)
```
heliosdb> \indexes orders
   suggested:  CREATE INDEX idx_orders_customer ON orders (customer_id);
   reason:     12 SeqScans in last 1000 queries;
                         estimated cost saving: ~85 %.
```
Then implement (`CREATE INDEX …`) and re-`EXPLAIN` to confirm.

### Recipe 6: Stats / compression
```
heliosdb> \stats
   tables:        12
   total rows:    8 312 405
   data size:     1.2 GiB
   wal size:      48 MiB
   cache hit:     94.3 %

heliosdb> \compression orders
   compressed: 78.4 % of raw
   algorithm:  zstd (level 3)
```

### Recipe 7: EXPLAIN ANALYZE for one query
```sql
EXPLAIN ANALYZE
SELECT u.email, COUNT(p.id) FROM users u
  JOIN posts p ON p.author_id = u.id
 GROUP BY u.email
 ORDER BY 2 DESC LIMIT 10;
```
Read top to bottom: estimated rows vs. actual, time per node, cache-hit ratios.

### Recipe 8: Wire `/health` to Prometheus / external monitoring
There is no native Prometheus exporter built into Nano-Lite today; the standard pattern is:
1. Run `heliosdb-nano start --http-port 8080 …`.
2. Use a sidecar exporter (e.g., a tiny shell that polls `/health` + parses `\stats` over a privileged session) to expose `/metrics`.
3. (Larger HeliosDB editions ship native Prometheus + OTLP — see the `Helios Full` repo if you need both inline.)

### Recipe 9: Audit log
With `[audit] enabled = true` in `config.toml`, every privileged action lands in JSONL at `[audit] log_path`. Tail with:
```bash
tail -f ./logs/audit.jsonl | jq -c '{ts, user, op, target}'
```
See `docs/guides/audit.md` for the full event taxonomy.

## Pitfalls
- **`RUST_LOG` is read once at startup**. Changing it after start has no effect — restart to apply.
- **Slow-query threshold is hardcoded at 1 s** today. Tracking issue if you need it configurable.
- **`/health` returns 200 even during catastrophic states** (e.g., disk full but the process is alive). It's a liveness probe, not a deep readiness probe — pair with a custom readiness check that runs `SELECT 1` against the data path.
- **`\stats` is a synchronous scan**: don't poll it on hot-path nodes more than every few minutes.
- **Tracing with `trace`-level on busy nodes** can produce hundreds of MB / minute. Always scope `RUST_LOG` tightly in production.

## See also
- `heliosdb-nano-server` — `--http-port` flag, audit configuration.
- `heliosdb-nano-query` — `EXPLAIN [ANALYZE]`, query patterns.
- `docs/TRACING_GUIDE.md` — deeper tracing/diagnostics walkthrough.
- `docs/guides/audit.md` — audit-log event taxonomy.
