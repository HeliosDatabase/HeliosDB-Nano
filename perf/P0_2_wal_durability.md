# P0#2 — Drop the redundant per-statement WAL fsync (autocommit UPDATE/DELETE)

**Branch:** `perf/p0-p1-p2`  ·  disk regime (XFS on software-RAID `md`, fsync median ~11 ms)

## Problem

In the baseline, an autocommit `UPDATE`/`DELETE` that goes through the plan arm
appends a **per-statement logical WAL entry with an fsync** (`wal_sync_mode=Sync`)
*in addition to* the RocksDB `WriteBatch` written at commit. That fsync is the
sole thing capping durable throughput at the device fsync rate.

INSERT and PK-`UPDATE` already avoid this (they hit fast paths that don't append
the logical WAL), so **DELETE was the lone victim**: it has no fast path, so every
autocommit `DELETE` paid one fsync.

Measured (disk, N=5000, M=300):

| op | before (logical-WAL fsync) | note |
|---|---:|---|
| autocommit_insert | 18,560/s | fast path, already no logical-WAL |
| update_by_pk | 20,293/s | fast path, already no logical-WAL |
| **delete_by_pk** | **63/s (15,915 µs)** | plan arm → fsync per statement |

## Change

Autocommit `UPDATE`/`DELETE` now append the logical WAL entry **without a
per-statement fsync** (`append_nosync`) by default — the entry is still written,
so crash-recovery replay and logical replication stay consistent, but durability
relies on the RocksDB `WriteBatch` at commit (uniform with the INSERT path and
with explicit transactions). New config `storage.logical_wal_per_statement`
(default `false`) restores the legacy fsync-per-statement behavior.

A first attempt simply *skipped* the logical WAL for DELETE/UPDATE; that broke
`crash_recovery_e2e::test_crash_recovery_update_delete` because INSERTs still log
to the WAL, so replay re-applied inserts but not the (unlogged) delete — the
deleted row resurrected. Using `append_nosync` (keep the entry, drop only the
fsync) keeps replay consistent.

## Result (disk, N=5000, M=300)

| op | before | after | speedup |
|---|---:|---:|---:|
| **delete_by_pk** | **63/s** | **4,175/s** | **43×** |
| update_by_pk (plan arm) | fsync-bound | 21,010/s | — |

DELETE is now CPU-bound (≈ in-memory latency), not fsync-bound.

## Durability / replication note

This relaxes autocommit UPDATE/DELETE durability from *fsync-per-statement* to
*RocksDB-WAL-at-commit* — i.e. a process crash is safe (RocksDB recovers its WAL),
but a power-loss within RocksDB's flush window can lose the last few autocommit
mutations. This now matches the INSERT path (and SQLite WAL + `synchronous=NORMAL`).
Set `logical_wal_per_statement=true` for strict per-statement durability. (A
unified per-DML durability/group-commit config across INSERT/UPDATE/DELETE is the
right longer-term design — noted for the consolidated report.)

## Validation

`crash_recovery_e2e_test`, `wal_crash_recovery_tests`, `transaction_tests`,
`transaction_integration_tests` — all pass (crash_recovery_e2e 4, wal_crash_recovery 35, transaction 27, transaction_integration 18).
