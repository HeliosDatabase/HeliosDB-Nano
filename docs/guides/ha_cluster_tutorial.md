# HeliosDB Nano HA Cluster Tutorial

This tutorial guides you through deploying a HeliosDB Nano High Availability cluster and testing its innovative **Transparent Write Routing** feature.

## What You'll Learn

1. Deploy a 4-node HA cluster with Docker
2. Test transparent write routing from standbys to primary
3. Understand the behavior of different sync modes
4. Monitor cluster status using system views

## Cluster Architecture

```
                    ┌─────────────────────────────────────┐
                    │           APPLICATIONS              │
                    │  (can connect to ANY node)          │
                    └──────────┬────────────┬─────────────┘
                               │            │
         ┌─────────────────────┼────────────┼─────────────────────┐
         │                     │            │                     │
         ▼                     ▼            ▼                     ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│   PRIMARY       │  │  STANDBY-SYNC   │  │ STANDBY-SEMISYNC│  │  STANDBY-ASYNC  │
│   (port 15432)  │  │  (port 15442)   │  │  (port 15452)   │  │  (port 15462)   │
│                 │  │                 │  │                 │  │                 │
│  Writes: Local  │  │ Writes: Forward │  │ Writes: Forward │  │ Writes: Reject  │
│  Reads: Local   │  │ Reads: Local    │  │ Reads: Local    │  │ Reads: Local    │
└─────────────────┘  └────────┬────────┘  └────────┬────────┘  └─────────────────┘
         ▲                    │                    │
         │                    │                    │
         └────────────────────┴────────────────────┘
                    Transparent Write Routing
```

## Prerequisites

- Docker and Docker Compose installed
- psql client (for testing)
- ~4GB disk space for Docker images

## Step 1: Start the HA Cluster

```bash
cd /path/to/HeliosDB Nano/tests/docker

# Build images (first time only, or after code changes)
docker compose -f docker-compose.ha-cluster.yml build

# Start the cluster
docker compose -f docker-compose.ha-cluster.yml up -d

# Watch startup logs
docker compose -f docker-compose.ha-cluster.yml logs -f
```

Wait for all containers to show "healthy":

```bash
docker ps --filter "name=heliosdb" --format "table {{.Names}}\t{{.Status}}"
```

Expected output:
```
NAMES                       STATUS
heliosdb-test-runner        Up (healthy)
heliosdb-standby-async      Up (healthy)
heliosdb-observer           Up (healthy)
heliosdb-standby-semisync   Up (healthy)
heliosdb-standby-sync       Up (healthy)
heliosdb-primary            Up (healthy)
```

## Step 2: Connect to the Cluster

### Port Mappings

| Node | PostgreSQL Port | Sync Mode | Write Behavior |
|------|-----------------|-----------|----------------|
| primary | 15432 | - | Execute locally |
| standby-sync | 15442 | sync | Forward to primary |
| standby-semisync | 15452 | semi-sync | Forward to primary |
| standby-async | 15462 | async | Reject (read-only) |

### Connect from Host

```bash
# Connect to primary
psql -h localhost -p 15432

# Connect to sync standby
psql -h localhost -p 15442

# Connect to semi-sync standby
psql -h localhost -p 15452

# Connect to async standby
psql -h localhost -p 15462
```

### Connect from Test Runner Container

```bash
# Install psql in test-runner
docker exec heliosdb-test-runner apt-get update -qq
docker exec heliosdb-test-runner apt-get install -y -qq postgresql-client

# Connect to any node using Docker DNS
docker exec -it heliosdb-test-runner psql -h primary -p 5432
docker exec -it heliosdb-test-runner psql -h standby-sync -p 5432
docker exec -it heliosdb-test-runner psql -h standby-semisync -p 5432
docker exec -it heliosdb-test-runner psql -h standby-async -p 5432
```

## Step 3: Test Transparent Write Routing

### 3.1 Create a Table on Primary

```bash
psql -h localhost -p 15432 -c "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, created_by TEXT);"
```

### 3.2 Insert via SYNC Standby (Transparent Routing)

Connect to the sync standby and insert data:

```bash
psql -h localhost -p 15442 -c "INSERT INTO users VALUES (1, 'Alice', 'standby-sync');"
```

**Expected**: `INSERT 0 1` - The write is transparently routed to primary via TWR!

### 3.3 Insert via SEMI-SYNC Standby (Transparent Routing)

```bash
psql -h localhost -p 15452 -c "INSERT INTO users VALUES (2, 'Bob', 'standby-semisync');"
```

**Expected**: `INSERT 0 1` - Also routed to primary via TWR!

### 3.4 Insert via ASYNC Standby (Read-Only)

```bash
psql -h localhost -p 15462 -c "INSERT INTO users VALUES (3, 'Charlie', 'standby-async');"
```

**Expected Error**:
```
ERROR:  cannot execute write operations in read-only mode (async standby)
HINT:  Connect to the primary for write operations, or configure sync mode for transparent routing.
```

### 3.5 Verify Data on Primary

```bash
psql -h localhost -p 15432 -c "SELECT * FROM users ORDER BY id;"
```

**Expected**:
```
 id | name  |   created_by
----+-------+------------------
  1 | Alice | standby-sync
  2 | Bob   | standby-semisync
(2 rows)
```

### 3.6 Test UPDATE and DELETE

```bash
# Update via sync standby
psql -h localhost -p 15442 -c "UPDATE users SET name = 'Alice Updated' WHERE id = 1;"

# Delete via semi-sync standby
psql -h localhost -p 15452 -c "DELETE FROM users WHERE id = 2;"

# Verify on primary
psql -h localhost -p 15432 -c "SELECT * FROM users;"
```

## Step 4: Monitor Cluster Status

### 4.1 Check Node Status

Query `pg_replication_status` from any node:

```bash
# From primary
psql -h localhost -p 15432 -c "SELECT * FROM pg_replication_status;"

# From standby
psql -h localhost -p 15442 -c "SELECT * FROM pg_replication_status;"
```

### 4.2 View Connected Standbys (Primary Only)

```bash
psql -h localhost -p 15432 -c "SELECT * FROM pg_replication_standbys;"
```

### 4.3 Check Primary Connection (Standby Only)

```bash
psql -h localhost -p 15442 -c "SELECT * FROM pg_replication_primary;"
```

### 4.4 View Replication Metrics

```bash
psql -h localhost -p 15432 -c "SELECT * FROM pg_replication_metrics;"
```

## Step 5: Interactive REPL Session

Start an interactive session to explore the cluster:

```bash
# Connect to primary
psql -h localhost -p 15432

# In psql, try these commands:
heliosdb=> CREATE TABLE products (id INT, name TEXT, price DECIMAL);
heliosdb=> INSERT INTO products VALUES (1, 'Widget', 9.99);
heliosdb=> SELECT * FROM products;
heliosdb=> SELECT * FROM pg_replication_status;
heliosdb=> \q

# Now connect to sync standby and try the same INSERT
psql -h localhost -p 15442

heliosdb=> INSERT INTO products VALUES (2, 'Gadget', 19.99);  -- Forwarded!
heliosdb=> SELECT * FROM pg_replication_status;  -- Shows standby role
heliosdb=> \q

# Verify both rows exist on primary
psql -h localhost -p 15432 -c "SELECT * FROM products;"
```

## Step 6: Clean Up

```bash
# Stop the cluster (preserve data)
docker compose -f docker-compose.ha-cluster.yml down

# Stop and remove all data
docker compose -f docker-compose.ha-cluster.yml down -v
```

## Understanding Sync Modes

| Mode | DQL (SELECT) | DML (INSERT/UPDATE/DELETE) | Use Case |
|------|--------------|----------------------------|----------|
| **sync** | Execute locally | Forward to primary, wait for full sync | Critical data, strong consistency |
| **semi-sync** | Execute locally | Forward to primary, wait for ack | Balanced consistency/performance |
| **async** | Execute locally | Reject (read-only) | Read replicas, analytics |

## Troubleshooting

### Container Won't Start

```bash
# Check logs
docker logs heliosdb-primary
docker logs heliosdb-standby-sync

# Common issues:
# - Port conflicts: Check if ports 15432, 15442, etc. are in use
# - Network issues: Run "docker network prune -f" to clean up
```

### Write Routing Fails

```bash
# Check standby logs for forwarder status
docker logs heliosdb-standby-sync 2>&1 | grep -i "forwarder\|routing"

# Verify primary is accessible from standby
docker exec heliosdb-standby-sync curl -s http://primary:8080/health
```

### Connection Refused

```bash
# Verify container is running and healthy
docker ps --filter "name=heliosdb"

# Test connectivity
docker exec heliosdb-test-runner nc -zv primary 5432
```

## Architecture Deep Dive

### Transparent Write Routing Flow

```
Application → Standby-Sync (port 15442)
    │
    │ INSERT INTO users VALUES (1, 'Alice');
    │
    ▼
┌─────────────────────────────────┐
│ PostgreSQL Protocol Handler    │
│ - Detects INSERT (DML)         │
│ - Checks sync_mode: sync       │
│ - Invokes QueryForwarder       │
└─────────────┬───────────────────┘
              │
              ▼
┌─────────────────────────────────┐
│ QueryForwarder                  │
│ - Connects to primary:5432     │
│ - Sends query via PG protocol  │
│ - Receives result              │
└─────────────┬───────────────────┘
              │
              ▼
┌─────────────────────────────────┐
│ Primary (port 15432)           │
│ - Executes INSERT              │
│ - Returns "INSERT 0 1"         │
└─────────────┬───────────────────┘
              │
              ▼
Application ← "INSERT 0 1" (transparent!)
```

### Benefits

1. **Simplified Application Logic**: Connect to any node, writes auto-route
2. **Load Distribution**: Reads stay local, writes go to primary
3. **High Availability**: Applications work even when connected to standby
4. **Zero Configuration**: No separate read/write connection pools needed

## Next Steps

- Explore [HA_TESTING_GUIDE.md](./HA_TESTING_GUIDE.md) for advanced testing scenarios
- Learn about [failover procedures](./HA_TESTING_GUIDE.md#test-8-simulate-node-failure)
- Configure [custom sync modes](./HA_TESTING_GUIDE.md#environment-variables) for your use case
