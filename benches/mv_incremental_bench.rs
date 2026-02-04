//! Benchmark for incremental materialized view refresh
//!
//! Compares performance between full recomputation and incremental refresh.

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use heliosdb_lite::{
    Config, StorageEngine, Schema, Column, DataType, Tuple, Value,
};
use heliosdb_lite::storage::{
    IncrementalRefresher, DeltaTracker, MaterializedViewMetadata,
};
use heliosdb_lite::sql::LogicalPlan;
use std::sync::Arc;

fn setup_test_data(storage: &StorageEngine, num_rows: usize) {
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("value", DataType::Int8),
        Column::new("status", DataType::Text),
    ]);

    let catalog = storage.catalog();
    let _ = catalog.create_table("test_table", schema);

    for i in 0..num_rows {
        let tuple = Tuple {
            values: vec![
                Value::Int4(i as i32),
                Value::Int8(i as i64 * 100),
                Value::String(if i % 2 == 0 { "active" } else { "inactive" }.to_string()),
            ],
        };
        let _ = storage.insert_tuple("test_table", tuple);
    }
}

fn benchmark_full_refresh(c: &mut Criterion) {
    let mut group = c.benchmark_group("materialized_view_refresh");

    for size in [100, 1000, 10_000].iter() {
        group.bench_with_input(
            BenchmarkId::new("full_refresh", size),
            size,
            |b, &size| {
                b.iter(|| {
                    let config = Config::in_memory();
                    let storage = StorageEngine::open_in_memory(&config).unwrap();
                    setup_test_data(&storage, size);

                    // Simulate full table scan
                    let tuples = storage.scan_table("test_table").unwrap();
                    black_box(tuples.len());
                });
            },
        );
    }

    group.finish();
}

fn benchmark_incremental_refresh(c: &mut Criterion) {
    let mut group = c.benchmark_group("materialized_view_refresh");

    for size in [100, 1000, 10_000].iter() {
        group.bench_with_input(
            BenchmarkId::new("incremental_refresh", size),
            size,
            |b, &size| {
                b.iter(|| {
                    let config = Config::in_memory();
                    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
                    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

                    setup_test_data(&storage, size);

                    // Simulate small number of changes (1% of dataset)
                    let num_changes = (size / 100).max(1);
                    for i in 0..num_changes {
                        let tuple = Tuple {
                            values: vec![
                                Value::Int4(i as i32),
                                Value::Int8(i as i64 * 200),
                                Value::String("updated".to_string()),
                            ],
                        };
                        tracker.record_insert("test_table", tuple, 1000 + i as u64);
                    }

                    // Get deltas and process them
                    let deltas = tracker.get_deltas_since("test_table", 500);
                    black_box(deltas.len());
                });
            },
        );
    }

    group.finish();
}

fn benchmark_delta_tracking(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_tracking");

    for num_deltas in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("record_deltas", num_deltas),
            num_deltas,
            |b, &num_deltas| {
                b.iter(|| {
                    let config = Config::in_memory();
                    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
                    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

                    for i in 0..num_deltas {
                        let tuple = Tuple {
                            values: vec![Value::Int4(i as i32)],
                        };
                        tracker.record_insert("test_table", tuple, i as u64);
                    }

                    black_box(tracker.count_deltas_since(&vec!["test_table".to_string()], 0).unwrap());
                });
            },
        );
    }

    group.finish();
}

fn benchmark_cost_estimation(c: &mut Criterion) {
    let mut group = c.benchmark_group("cost_estimation");

    group.bench_function("estimate_refresh_cost", |b| {
        b.iter(|| {
            let config = Config::in_memory();
            let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
            let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));
            let refresher = IncrementalRefresher::new(Arc::clone(&storage), tracker);

            setup_test_data(&storage, 1000);

            let schema = Schema::new(vec![
                Column::new("id", DataType::Int4),
                Column::new("value", DataType::Int8),
                Column::new("status", DataType::Text),
            ]);

            let query_plan = LogicalPlan::Scan {
                table_name: "test_table".to_string(),
                schema: Arc::new(schema.clone()),
                projection: None,
                as_of: None,
            };
            let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

            let mut mv_metadata = MaterializedViewMetadata::new(
                "test_view".to_string(),
                "SELECT * FROM test_table".to_string(),
                query_plan_bytes,
                vec!["test_table".to_string()],
                schema,
            );

            mv_metadata.mark_refreshed(1000);

            let cost = refresher.estimate_refresh_cost(&mv_metadata).unwrap();
            black_box(cost);
        });
    });

    group.finish();
}

fn benchmark_aggregate_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate_operations");

    for num_updates in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("incremental_aggregate", num_updates),
            num_updates,
            |b, &num_updates| {
                b.iter(|| {
                    let config = Config::in_memory();
                    let storage = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
                    let tracker = Arc::new(DeltaTracker::new(Arc::clone(&storage)));

                    // Simulate aggregate updates (COUNT, SUM operations)
                    for i in 0..num_updates {
                        let tuple = Tuple {
                            values: vec![
                                Value::Int4((i % 10) as i32), // Group key
                                Value::Int8(i as i64),        // Value to aggregate
                            ],
                        };
                        tracker.record_insert("test_table", tuple, i as u64);
                    }

                    let deltas = tracker.get_deltas_since("test_table", 0);
                    black_box(deltas.len());
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_full_refresh,
    benchmark_incremental_refresh,
    benchmark_delta_tracking,
    benchmark_cost_estimation,
    benchmark_aggregate_incremental,
);

criterion_main!(benches);
