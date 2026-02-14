//! Benchmarks for Time-Travel Query Optimization
//!
//! Compares linear scan O(N) vs reverse index O(log N) performance
//! for AS OF TIMESTAMP queries.

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use heliosdb_nano::storage::time_travel::SnapshotManager;
use rocksdb::DB;
use std::sync::Arc;
use tempfile::tempdir;

/// Create a test database with many historical versions
fn create_db_with_versions(num_versions: usize) -> (Arc<DB>, tempfile::TempDir, SnapshotManager) {
    let temp_dir = tempdir().unwrap();
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);

    // Enable bloom filters for better index performance
    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_bloom_filter(10.0, false);
    opts.set_block_based_table_factory(&block_opts);

    let db = DB::open(&opts, temp_dir.path()).unwrap();
    let db_arc = Arc::new(db);
    let manager = SnapshotManager::new(db_arc.clone());

    // Insert versions for a single row at different timestamps
    let table_name = "test_table";
    let row_id = 1u64;

    for i in 0..num_versions {
        let timestamp = (i + 1) as u64 * 1000; // Timestamps: 1000, 2000, 3000, ...
        let value = format!("value_at_{}", timestamp);
        manager.write_version(table_name, row_id, timestamp, value.as_bytes())
            .unwrap();
    }

    (db_arc, temp_dir, manager)
}

/// Benchmark linear scan (old method)
fn bench_linear_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("linear_scan");

    for num_versions in [10, 100, 1000, 10000] {
        let (_db, _temp, manager) = create_db_with_versions(num_versions);
        group.throughput(Throughput::Elements(num_versions as u64));

        group.bench_with_input(
            BenchmarkId::new("linear", num_versions),
            &num_versions,
            |b, &size| {
                // Query at 75% of the timeline
                let target_ts = (size as u64 * 1000 * 3) / 4;
                b.iter(|| {
                    let result = manager.read_at_snapshot_linear(
                        black_box("test_table"),
                        black_box(1),
                        black_box(target_ts),
                    ).unwrap();
                    black_box(result)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark indexed lookup (new method)
fn bench_indexed_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("indexed_lookup");

    for num_versions in [10, 100, 1000, 10000] {
        let (_db, _temp, manager) = create_db_with_versions(num_versions);
        group.throughput(Throughput::Elements(num_versions as u64));

        group.bench_with_input(
            BenchmarkId::new("indexed", num_versions),
            &num_versions,
            |b, &size| {
                // Query at 75% of the timeline
                let target_ts = (size as u64 * 1000 * 3) / 4;
                b.iter(|| {
                    let result = manager.read_at_snapshot(
                        black_box("test_table"),
                        black_box(1),
                        black_box(target_ts),
                    ).unwrap();
                    black_box(result)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark comparison: linear vs indexed
fn bench_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("comparison");

    for num_versions in [100, 1000, 10000] {
        let (_db, _temp, manager) = create_db_with_versions(num_versions);
        let target_ts = (num_versions as u64 * 1000 * 3) / 4;

        // Linear scan
        group.bench_with_input(
            BenchmarkId::new("linear", num_versions),
            &num_versions,
            |b, _| {
                b.iter(|| {
                    let result = manager.read_at_snapshot_linear(
                        black_box("test_table"),
                        black_box(1),
                        black_box(target_ts),
                    ).unwrap();
                    black_box(result)
                });
            },
        );

        // Indexed lookup
        group.bench_with_input(
            BenchmarkId::new("indexed", num_versions),
            &num_versions,
            |b, _| {
                b.iter(|| {
                    let result = manager.read_at_snapshot(
                        black_box("test_table"),
                        black_box(1),
                        black_box(target_ts),
                    ).unwrap();
                    black_box(result)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark different query positions (beginning, middle, end)
fn bench_query_positions(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_positions");
    let num_versions = 1000;
    let (_db, _temp, manager) = create_db_with_versions(num_versions);

    // Query at beginning (10%)
    group.bench_function("indexed_10pct", |b| {
        let target_ts = (num_versions as u64 * 1000) / 10;
        b.iter(|| {
            let result = manager.read_at_snapshot(
                black_box("test_table"),
                black_box(1),
                black_box(target_ts),
            ).unwrap();
            black_box(result)
        });
    });

    // Query at middle (50%)
    group.bench_function("indexed_50pct", |b| {
        let target_ts = (num_versions as u64 * 1000) / 2;
        b.iter(|| {
            let result = manager.read_at_snapshot(
                black_box("test_table"),
                black_box(1),
                black_box(target_ts),
            ).unwrap();
            black_box(result)
        });
    });

    // Query at end (90%)
    group.bench_function("indexed_90pct", |b| {
        let target_ts = (num_versions as u64 * 1000 * 9) / 10;
        b.iter(|| {
            let result = manager.read_at_snapshot(
                black_box("test_table"),
                black_box(1),
                black_box(target_ts),
            ).unwrap();
            black_box(result)
        });
    });

    group.finish();
}

/// Benchmark write overhead (index creation cost)
fn bench_write_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_overhead");

    let temp_dir = tempdir().unwrap();
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);

    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_bloom_filter(10.0, false);
    opts.set_block_based_table_factory(&block_opts);

    let db = DB::open(&opts, temp_dir.path()).unwrap();
    let db_arc = Arc::new(db);
    let manager = SnapshotManager::new(db_arc);

    let mut counter = 0u64;

    group.bench_function("write_with_index", |b| {
        b.iter(|| {
            counter += 1;
            let timestamp = counter * 1000;
            let value = format!("value_{}", timestamp);
            manager.write_version(
                black_box("test_table"),
                black_box(1),
                black_box(timestamp),
                black_box(value.as_bytes()),
            ).unwrap();
        });
    });

    group.finish();
}

/// Benchmark multi-row queries
fn bench_multi_row(c: &mut Criterion) {
    let mut group = c.benchmark_group("multi_row");

    let temp_dir = tempdir().unwrap();
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);

    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_bloom_filter(10.0, false);
    opts.set_block_based_table_factory(&block_opts);

    let db = DB::open(&opts, temp_dir.path()).unwrap();
    let db_arc = Arc::new(db);
    let manager = SnapshotManager::new(db_arc);

    // Create versions for multiple rows
    let num_rows = 100;
    let versions_per_row = 100;

    for row_id in 1..=num_rows {
        for version in 1..=versions_per_row {
            let timestamp = (version * 1000) as u64;
            let value = format!("row_{}_value_{}", row_id, timestamp);
            manager.write_version("test_table", row_id, timestamp, value.as_bytes())
                .unwrap();
        }
    }

    // Benchmark querying all rows at a specific timestamp
    group.bench_function("query_all_rows_indexed", |b| {
        let target_ts = 50000; // Middle of timeline
        b.iter(|| {
            let mut results = Vec::new();
            for row_id in 1..=num_rows {
                let result = manager.read_at_snapshot(
                    black_box("test_table"),
                    black_box(row_id),
                    black_box(target_ts),
                ).unwrap();
                results.push(result);
            }
            black_box(results)
        });
    });

    group.finish();
}

/// Benchmark edge cases
fn bench_edge_cases(c: &mut Criterion) {
    let mut group = c.benchmark_group("edge_cases");
    let (_db, _temp, manager) = create_db_with_versions(100);

    // Query before first version
    group.bench_function("before_first_version", |b| {
        b.iter(|| {
            let result = manager.read_at_snapshot(
                black_box("test_table"),
                black_box(1),
                black_box(500), // Before first version at 1000
            ).unwrap();
            black_box(result)
        });
    });

    // Query after last version
    group.bench_function("after_last_version", |b| {
        b.iter(|| {
            let result = manager.read_at_snapshot(
                black_box("test_table"),
                black_box(1),
                black_box(1000000), // After last version at 100000
            ).unwrap();
            black_box(result)
        });
    });

    // Query exact version match
    group.bench_function("exact_match", |b| {
        b.iter(|| {
            let result = manager.read_at_snapshot(
                black_box("test_table"),
                black_box(1),
                black_box(50000), // Exact match
            ).unwrap();
            black_box(result)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_linear_scan,
    bench_indexed_lookup,
    bench_comparison,
    bench_query_positions,
    bench_write_overhead,
    bench_multi_row,
    bench_edge_cases,
);

criterion_main!(benches);
