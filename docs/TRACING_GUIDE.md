# HeliosDB Nano — Performance Tracing Guide

## Overview

HeliosDB Nano includes structured performance tracing across the entire query pipeline. This guide explains how to enable, configure, and analyze tracing output for performance analysis.

## Quick Start

```bash
# Enable all debug-level tracing
RUST_LOG=debug heliosdb-proxy

# Enable only performance-relevant phases
RUST_LOG=heliosdb_nano=debug heliosdb-proxy

# Enable trace-level for maximum detail (includes txn_begin)
RUST_LOG=heliosdb_nano=trace heliosdb-proxy

# Combine with slow query log (queries > 1s logged at WARN)
RUST_LOG=heliosdb_nano=debug,warn heliosdb-proxy
```

## Instrumented Pipeline

Every SQL query flows through these instrumented phases:

```
Client Query
    │
    ├─ [parse]           SQL text → AST           (debug, duration_us)
    ├─ [plan]            AST → LogicalPlan        (debug, duration_us)
    ├─ [txn_begin]       Transaction start         (trace, duration_us)
    ├─ [execute]         Plan execution            (debug, duration_us)
    │   ├─ [operator_build]  Plan → Operator tree  (debug, duration_us, plan_type)
    │   ├─ [operator_exec]   Operator iteration    (debug, duration_us, rows)
    │   └─ [storage_scan]    RocksDB table scan    (debug, duration_us, table, rows)
    ├─ [txn_commit]      Transaction commit         (debug, duration_us, rows)
    └─ [slow_query]      If duration > threshold    (warn, duration_ms, rows, sql)
```

## Tracing Fields

Each tracing event includes structured fields for filtering and analysis:

| Field | Type | Description |
|-------|------|-------------|
| `phase` | string | Pipeline stage (parse, plan, execute, etc.) |
| `duration_us` | u64 | Duration in microseconds |
| `duration_ms` | u64 | Duration in milliseconds (slow query log only) |
| `rows` | usize | Number of rows processed/returned |
| `plan_type` | string | LogicalPlan variant (Scan, FilteredScan, Join, etc.) |
| `table` | string | Table name (storage_scan only) |

## Slow Query Log

Queries exceeding the configured threshold are logged at WARN level automatically.

### Configuration

In `heliosdb.toml`:
```toml
[storage]
slow_query_threshold_ms = 1000  # Default: 1000ms (1 second)
```

Or via SQL:
```sql
-- View current setting
SELECT * FROM pg_settings WHERE name = 'slow_query_threshold_ms';
```

### Output Format

```
WARN Slow query (1523ms, 10000 rows): SELECT * FROM large_table WHERE ...
```

The SQL is truncated to 200 characters to prevent log bloat.

## Performance Analysis Recipes

### 1. Find Slow Scans

```bash
RUST_LOG=heliosdb_nano=debug heliosdb-proxy 2>&1 | grep 'phase.*storage_scan'
```

Example output:
```
DEBUG phase="storage_scan" table="orders" rows=50000 duration_us=45200
```

### 2. Identify Planning Bottlenecks

```bash
RUST_LOG=heliosdb_nano=debug heliosdb-proxy 2>&1 | grep 'phase.*plan'
```

If `plan` duration_us is high relative to `execute`, consider:
- Simplifying complex CTEs or subqueries
- Reducing the number of JOINs

### 3. Compare Operator Build vs Execute Time

```bash
RUST_LOG=heliosdb_nano=debug heliosdb-proxy 2>&1 | grep 'phase.*operator'
```

- High `operator_build` → complex plan tree (many operators)
- High `operator_exec` → actual data processing bottleneck

### 4. Monitor Transaction Overhead

```bash
RUST_LOG=heliosdb_nano=trace heliosdb-proxy 2>&1 | grep 'phase.*txn'
```

High `txn_commit` duration may indicate:
- Lock contention under concurrent load
- Large write batches being flushed

### 5. Capture Full Query Profile

```bash
RUST_LOG=heliosdb_nano=trace heliosdb-proxy 2>&1 | tee query_trace.log
```

Then analyze with:
```bash
# Find the 10 slowest operations
grep 'duration_us' query_trace.log | \
  sed 's/.*duration_us=\([0-9]*\).*/\1/' | \
  sort -rn | head -10

# Count operations by phase
grep -oP 'phase="\K[^"]+' query_trace.log | sort | uniq -c | sort -rn
```

## Programmatic Access (Embedded Mode)

When using HeliosDB Nano as a library, configure tracing in your application:

```rust
use heliosdb_nano::EmbeddedDatabase;
use tracing_subscriber::EnvFilter;

fn main() {
    // Initialize tracing subscriber
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("heliosdb_nano=debug"))
        .init();

    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // All queries now emit structured tracing events
    db.execute("CREATE TABLE test (id INT, name TEXT)").unwrap();
    db.execute("INSERT INTO test VALUES (1, 'hello')").unwrap();

    // This will emit: parse → plan → txn_begin → execute → operator_build →
    //                  operator_exec → storage_scan → txn_commit
    let _ = db.query("SELECT * FROM test", &[]);
}
```

### Custom Tracing Subscribers

For JSON output (useful for log aggregation):

```rust
tracing_subscriber::fmt()
    .json()
    .with_env_filter(EnvFilter::new("heliosdb_nano=debug"))
    .init();
```

For file output:

```rust
use std::fs::File;
use tracing_subscriber::fmt::writer::MakeWriterExt;

let file = File::create("heliosdb_trace.log").unwrap();
tracing_subscriber::fmt()
    .with_writer(file)
    .with_env_filter(EnvFilter::new("heliosdb_nano=debug"))
    .init();
```

## Connection & Session Tracing

Session-level events are also traced:

| Event | Level | Fields |
|-------|-------|--------|
| Session start | INFO | session_id |
| Session close | INFO | session_id |
| Idle timeout disconnect | INFO | session_id, idle_timeout_secs |
| Query timeout | ERROR | session_id, timeout_ms |
| Connection limit reached | WARN | max_connections |

## Configuration Reference

| Setting | Default | Description |
|---------|---------|-------------|
| `storage.slow_query_threshold_ms` | 1000 | Slow query log threshold (ms). `null` to disable |
| `server.idle_timeout_secs` | 300 | Idle connection timeout (seconds). 0 to disable |
| `RUST_LOG` env var | (none) | Controls tracing verbosity per module |
