# External (wire-protocol) Benchmarks

Scripts here connect to a **running** server over the PostgreSQL or MySQL
wire protocol and measure end-to-end query latency. They're
engine-agnostic: point them at HeliosDB Nano, PostgreSQL, CockroachDB,
YugabyteDB, or any other PG-wire-compatible backend.

## Requirements

```bash
pip install 'psycopg[binary]'
```

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
