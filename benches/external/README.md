# External (wire-protocol) Benchmarks

Scripts here connect to a **running** server over the PostgreSQL or MySQL
wire protocol and measure end-to-end query latency. They're
engine-agnostic: point them at HeliosDB Nano, PostgreSQL, CockroachDB,
YugabyteDB, or any other PG-wire-compatible backend.

## Requirements

```bash
pip install 'psycopg[binary]'
```

For hosts that do not have `psql`, `mysql`, or Python DB drivers installed,
`docker_sql_tps_mirror.py` can drive the client binaries inside existing
PostgreSQL/MariaDB containers using only Python's standard library.
For Docker-server apples-to-apples checks, prefer `--client-mode client-container`
with a long-lived client container sharing the target server container's network
namespace; `--client-mode network-container` is only a smoke-test mode because it
starts a fresh client container for every timed workload.

## Pagination benchmark (`pagination_bench.py`)

Measures p50/p95/p99 latency for the three pagination shapes that matter
most for LOB applications (CRM, ERP, admin UIs):

1. **Offset** — `SELECT … ORDER BY id LIMIT 10 OFFSET M` at M = 0, 100, 1k, 10k, 99.99k
2. **Keyset** — `SELECT … WHERE id > $last ORDER BY id LIMIT 10`
3. **Join + offset** — `LEFT OUTER JOIN … LIMIT 10 OFFSET M`
4. **Tuple keyset** — `WHERE (created_at, id) < ($1, $2) ORDER BY … LIMIT 10`

### Run against HeliosDB Nano

```bash
# Start Nano in one terminal
cargo build --release
./target/release/heliosdb-nano start --memory --pg-socket-dir /tmp --port 5432

# In another terminal — Unix socket
python3 benches/external/pagination_bench.py \
    --host /tmp --port 5432 --user postgres --dbname heliosdb \
    --name "HeliosDB Nano" --rows 100000 \
    --out nano.json
```

### Run against PostgreSQL

```bash
PGPASSWORD=postgres python3 benches/external/pagination_bench.py \
    --host localhost --port 5432 --user postgres \
    --name "PostgreSQL 16" --rows 100000 \
    --out pg16.json
```

### Side-by-side comparison

```bash
python3 benches/external/pagination_bench.py --compare nano.json pg16.json
```

### Published results

See `Website/site/pagination-performance.html` for an annotated version
of the output. HeliosDB Nano 3.12.0 delivers constant-time pagination
(~32 µs) regardless of offset depth — up to **334× faster** than
PostgreSQL 13 for `OFFSET 99990` on a 100k-row table.

## Other scripts

- `pg_vs_helios.py` — broader PostgreSQL comparison (10 query
  categories, not pagination-focused).
- `sqlite_tps_mirror.py` — SQLite mirror of `tests/tps_workloads.rs`.
- `docker_sql_tps_mirror.py` — PostgreSQL/MariaDB Docker-client mirror of
  `tests/tps_workloads.rs`, useful for same-host external checks when only
  database containers are available.

Example read/analytics runs:

```bash
python3 benches/external/docker_sql_tps_mirror.py \
  --backend postgres --container postgres-primary \
  --user helios --password helios --database heliosdb \
  --n 10000 --m 2000 \
  --workloads filter_scan,agg_count_sum_avg,group_by_status,join_users_orders,order_by_limit10

python3 benches/external/docker_sql_tps_mirror.py \
  --backend mysql --container hdb-sprint-gitea-mysql-db \
  --user gitea --password gitea --database gitea \
  --n 10000 --m 2000 \
  --workloads filter_scan,agg_count_sum_avg,group_by_status,join_users_orders,order_by_limit10
```

Example apples-to-apples Docker client-container run against a PG-wire server:

```bash
docker run -d --name codex-nano-pg-client \
  --network container:codex-nano-tps postgres:17-bookworm sleep infinity

python3 benches/external/docker_sql_tps_mirror.py \
  --backend postgres --container codex-nano-tps \
  --client-mode client-container --client-container codex-nano-pg-client \
  --user postgres --password nano --database heliosdb \
  --n 10000 --m 2000 \
  --workloads filter_scan,agg_count_sum_avg,group_by_status,join_users_orders,order_by_limit10
```
