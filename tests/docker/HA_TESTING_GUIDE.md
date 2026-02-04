# HeliosDB-Lite HA Cluster Manual Testing Guide

This guide documents how to manually test the HA (High Availability) Tier 1 implementation.

## Cluster Architecture

The docker-compose cluster consists of:
- **Primary** (1 node): Handles all writes, streams WAL to standbys
- **Standbys** (2 nodes): Connected for replication, read-only in production
- **Observer** (1 node): Witness node for split-brain protection
- **Test-runner**: Container for running tests against the cluster

## Quick Start

### 1. Start the Cluster

```bash
cd /home/app/HeliosDB-Lite/tests/docker

# Start all nodes
docker compose -f docker-compose.ha-cluster.yml up -d

# Watch logs
docker compose -f docker-compose.ha-cluster.yml logs -f
```

### 2. Verify Cluster Health

```bash
# Check all containers are healthy
docker ps --filter "name=heliosdb" --format "table {{.Names}}\t{{.Status}}"

# Test health endpoints
curl http://localhost:18080/health  # Primary
curl http://localhost:18081/health  # Standby1
curl http://localhost:18082/health  # Standby2
curl http://localhost:18083/health  # Observer
```

### 3. Stop the Cluster

```bash
# Stop and remove containers (preserve volumes)
docker compose -f docker-compose.ha-cluster.yml down

# Stop and remove everything including volumes
docker compose -f docker-compose.ha-cluster.yml down -v
```

## Port Mappings

| Service  | PostgreSQL | Replication | HTTP Health |
|----------|------------|-------------|-------------|
| Primary  | 15432      | 15433       | 18080       |
| Standby1 | 15442      | 15443       | 18081       |
| Standby2 | 15452      | 15453       | 18082       |
| Observer | -          | 15463       | 18083       |

## Testing Scenarios

### Test 1: Verify Replication Connections

Check that standbys have connected to primary:

```bash
# View primary logs for standby registrations
docker logs heliosdb-primary 2>&1 | grep -i "standby.*registered"

# Expected output:
# Standby <uuid> registered
```

### Test 2: Connect to Primary

Using the test-runner container (psql must be installed first):

```bash
# Install psql in test-runner
docker exec heliosdb-test-runner apt-get update -qq
docker exec heliosdb-test-runner apt-get install -y -qq postgresql-client

# Connect and run queries on primary
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "CREATE TABLE users (id INT, name TEXT);"
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');"
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT * FROM users;"
```

Or from host (using mapped ports):

```bash
psql -h localhost -p 15432 -c "SELECT 1;"
```

### Test 3: Test Network Connectivity

```bash
# From test-runner, test connectivity to all nodes
docker exec heliosdb-test-runner curl -s http://primary:8080/health
docker exec heliosdb-test-runner curl -s http://standby1:8080/health
docker exec heliosdb-test-runner curl -s http://standby2:8080/health
docker exec heliosdb-test-runner curl -s http://observer:8080/health
```

### Test 4: View Replication Status (Logs)

```bash
# Check streaming connections on primary
docker logs heliosdb-primary 2>&1 | grep -E "(Streaming|Standby|connection)"

# Check standby connection status
docker logs heliosdb-standby1 2>&1 | grep -E "(Connected|primary|LSN)"
docker logs heliosdb-standby2 2>&1 | grep -E "(Connected|primary|LSN)"
```

### Test 5: Monitor HA Status with System Views

HeliosDB-Lite provides SQL system views for monitoring HA configuration and replication metrics:

#### Check Node Status (Primary or Standby)

```bash
# From primary - view node status
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT * FROM pg_replication_status;"

# From standby - view node status (read-only queries work)
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "SELECT * FROM pg_replication_status;"
```

Expected output columns:
- `node_id`: Unique identifier for this node
- `role`: primary, standby, observer, or standalone
- `sync_mode`: async, semi-sync, or sync
- `listen_address`: Host and port
- `replication_port`: WAL streaming port
- `current_lsn`: Current log sequence number
- `is_read_only`: true/false
- `standby_count`: Number of connected standbys (primary only)
- `uptime_seconds`: Time since node started

#### View Connected Standbys (Primary Only)

```bash
# From primary - see all connected standbys
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT * FROM pg_replication_standbys;"
```

Expected output columns:
- `node_id`: Standby's unique identifier
- `address`: Standby's connection address
- `sync_mode`: Replication mode for this standby
- `state`: connecting, streaming, catching_up, synced, disconnected
- `current_lsn`, `flush_lsn`, `apply_lsn`: LSN progress
- `lag_bytes`: Replication lag in bytes
- `lag_ms`: Replication lag in milliseconds
- `connected_at`, `last_heartbeat`: Timestamps

#### View Primary Connection (Standby Only)

```bash
# From standby - see primary connection status
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "SELECT * FROM pg_replication_primary;"
```

Expected output columns:
- `node_id`: Primary's unique identifier
- `address`: Primary's address
- `state`: disconnected, connecting, connected, streaming, error
- `primary_lsn`, `local_lsn`: LSN tracking
- `lag_bytes`, `lag_ms`: Replication lag
- `fencing_token`: Split-brain protection token
- `connected_at`, `last_heartbeat`: Timestamps

#### View Replication Metrics

```bash
# From any node - view performance metrics
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT * FROM pg_replication_metrics;"
```

Expected output columns:
- `wal_writes`: Total WAL write operations
- `wal_bytes_written`: Total WAL bytes written
- `records_replicated`: Records sent to standbys
- `bytes_replicated`: Bytes sent to standbys
- `heartbeats_sent`, `heartbeats_received`: Health check counts
- `reconnect_count`: Number of reconnections
- `last_wal_write`, `last_replication`: Timestamps

### Test 6: Transparent Write Routing (HeliosProxy Feature)

HeliosDB-Lite implements **Transparent Write Routing (TWR)** - an innovative feature that allows applications to connect to any node (primary or standby) and have writes automatically routed to the primary.

#### Behavior by Sync Mode

| Sync Mode | DQL (SELECT) | DML (INSERT/UPDATE/DELETE) |
|-----------|--------------|----------------------------|
| **sync** | Execute locally on standby | Forward to primary, return result |
| **semi-sync** | Execute locally on standby | Forward to primary, return result |
| **async** | Execute locally on standby | Reject (traditional read-only) |

#### Test Transparent Routing (Sync/Semi-Sync Mode)

With sync or semi-sync mode, writes to standbys are transparently routed to primary:

```bash
# Connect to STANDBY and execute INSERT (forwarded to primary)
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "INSERT INTO users VALUES (3, 'Charlie');"
# Result: INSERT 0 1 (success - executed on primary)

# Verify the data exists on primary
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT * FROM users;"

# UPDATE through standby (forwarded to primary)
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "UPDATE users SET name = 'Charles' WHERE id = 3;"

# DELETE through standby (forwarded to primary)
docker exec heliosdb-test-runner psql -h standby2 -p 5432 -c "DELETE FROM users WHERE id = 3;"

# DDL operations also forwarded
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "CREATE TABLE forwarded_test (id INT);"
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "DROP TABLE forwarded_test;"

# SELECT always executes locally on the connected standby
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "SELECT * FROM users;"
```

#### Test Async Mode (Traditional Read-Only)

With async mode, standbys reject write operations:

```bash
# Configure standby with async mode (in docker-compose or env)
# HELIOSDB_SYNC_MODE=async

# Try to INSERT on async standby (will fail)
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "INSERT INTO users VALUES (3, 'Charlie');"

# Expected error:
# ERROR:  cannot execute write operations in read-only mode (async standby)
# HINT:  Connect to the primary for write operations, or configure sync mode for transparent routing.

# Read operations still work
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "SELECT * FROM users;"
```

#### Benefits of Transparent Routing

1. **Load Distribution**: Applications can connect to any node; reads distributed, writes auto-routed
2. **Simplified Application Logic**: No need for separate read/write connection strings
3. **High Availability**: Application continues working if it connects to standby
4. **Transparent Failover**: Combined with connection pooling, provides seamless failover

#### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| HELIOSDB_SYNC_MODE | async | Controls routing: sync/semi-sync enable forwarding |
| HELIOSDB_PRIMARY_PG_PORT | 5432 | Primary's postgres port (for query forwarding) |

#### Operations Subject to Routing

When connected to a standby in sync/semi-sync mode:

| Operation | Behavior |
|-----------|----------|
| `SELECT` | Execute locally (DQL) |
| `INSERT` | Forward to primary (DML) |
| `UPDATE` | Forward to primary (DML) |
| `DELETE` | Forward to primary (DML) |
| `CREATE` | Forward to primary (DDL) |
| `DROP` | Forward to primary (DDL) |
| `ALTER` | Forward to primary (DDL) |
| `TRUNCATE` | Forward to primary (DDL) |

### Test 7: Verify Data Replication on Standbys

Once WAL streaming is fully implemented, data written to primary will be replicated to standbys:

```bash
# 1. Create table and insert data on primary
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "CREATE TABLE products (id INT, name TEXT, price DECIMAL);"
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "INSERT INTO products VALUES (1, 'Widget', 9.99), (2, 'Gadget', 19.99);"

# 2. Query the data on primary to confirm
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT * FROM products;"

# 3. Check replication lag on primary
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT node_id, state, lag_bytes, lag_ms FROM pg_replication_standbys;"

# 4. Once lag_bytes = 0 and state = 'synced', query standbys
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "SELECT * FROM products;"
docker exec heliosdb-test-runner psql -h standby2 -p 5432 -c "SELECT * FROM products;"

# 5. Verify data matches across all nodes
docker exec heliosdb-test-runner psql -h primary -p 5432 -c "SELECT COUNT(*) FROM products;"
docker exec heliosdb-test-runner psql -h standby1 -p 5432 -c "SELECT COUNT(*) FROM products;"
docker exec heliosdb-test-runner psql -h standby2 -p 5432 -c "SELECT COUNT(*) FROM products;"
```

Note: WAL streaming data replication is pending implementation. Currently, standbys connect and register but don't receive data changes.

### Test 8: Simulate Node Failure

```bash
# Stop standby1
docker stop heliosdb-standby1

# Check primary logs for disconnect
docker logs heliosdb-primary 2>&1 | tail -20

# Restart standby1
docker start heliosdb-standby1

# Verify reconnection
docker logs heliosdb-standby1 2>&1 | tail -10
```

## Current Implementation Status

### Working Features

1. **Streaming Protocol**: Primary accepts connections from standbys
2. **Node Registration**: Standbys register with primary via handshake
3. **Health Endpoints**: All nodes expose `/health` for monitoring
4. **Docker Networking**: Nodes communicate via Docker DNS
5. **Split-Brain Protection**: Fencing token infrastructure in place
6. **HA System Views**: SQL views for monitoring replication status
   - `pg_replication_status`: Node configuration and role
   - `pg_replication_standbys`: Connected standbys (primary)
   - `pg_replication_primary`: Primary connection (standbys)
   - `pg_replication_metrics`: Performance counters
7. **Transparent Write Routing (TWR)**: Innovative feature that routes DML/DDL from standbys to primary
   - Sync/Semi-sync mode: Writes transparently routed, results returned
   - Async mode: Writes rejected (traditional read-only standby)
   - DQL (SELECT) always executes locally on the connected node

### Pending Features (Future Work)

1. **WAL Streaming**: Actual WAL records not yet streamed to standbys
2. **Data Replication**: Changes on primary not yet applied to standbys
3. **Automatic Failover**: Manual promotion required
4. **Read Replica Queries**: Standbys don't yet serve read queries for primary data (need WAL streaming first)

## Troubleshooting

### Container Won't Start

```bash
# Check container logs
docker logs heliosdb-<node>

# Common issues:
# - Port already in use: Check for conflicting services
# - Primary not available: Standby waits 60s for primary
```

### Connection Refused

```bash
# Verify node is running
docker ps --filter "name=heliosdb"

# Check what ports are listening inside container
docker exec heliosdb-primary cat /proc/net/tcp
```

### Replication Not Connected

```bash
# Check network connectivity between containers
docker exec heliosdb-standby1 nc -z primary 5433

# Verify primary is listening on replication port
docker exec heliosdb-primary cat /proc/net/tcp | grep 1539  # 0x1539 = 5433
```

## Log Interpretation

### Successful Startup (Primary)
```
Streaming server listening on 0.0.0.0:5433
New connection from 172.28.1.2:xxxxx
Handshake from Standby node <uuid>
Standby <uuid> registered
```

### Successful Startup (Standby)
```
Streaming client connecting to primary at primary:5433
Connected to primary (node: <uuid>, LSN: 0, fencing: 1)
```

### Connection Issues
```
# Early EOF - usually from health check probes (normal)
Connection error: Transport error: Failed to read header: early eof

# Cannot resolve - DNS issue
Cannot resolve primary host 'primary:5433'
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| HELIOSDB_ROLE | primary | Node role: primary, standby, observer |
| HELIOSDB_DATA_DIR | /data | Data directory path |
| HELIOSDB_PORT | 5432 | PostgreSQL protocol port |
| HELIOSDB_REPL_PORT | 5433 | Replication streaming port |
| HELIOSDB_HTTP_PORT | 8080 | Health check HTTP port |
| HELIOSDB_PRIMARY_HOST | - | Primary host:port (for standbys) |
| HELIOSDB_PRIMARY_PG_PORT | 5432 | Primary's postgres port (for query forwarding) |
| HELIOSDB_SYNC_MODE | async | Replication mode: async, semi-sync, sync |
| HELIOSDB_LOG_LEVEL | info | Log level (via RUST_LOG) |

**Note on HELIOSDB_SYNC_MODE**: This setting affects write behavior on standbys:
- `async`: Traditional read-only standby; writes rejected
- `semi-sync`: Transparent Write Routing (TWR); writes routed to primary
- `sync`: Transparent write routing with full synchronization

## Running Integration Tests

The HA integration tests can be run directly:

```bash
cd /home/app/HeliosDB-Lite
cargo test --features ha-tier1 --test ha_integration
```

These tests run in-process without Docker and verify:
- WAL store operations
- Streaming protocol
- Transport layer
- Split-brain protection
- Logical replication
