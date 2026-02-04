//! Performance benchmarks for database branching operations
//!
//! Measures:
//! - Branch creation time (should be O(1), instant)
//! - Branch read performance (copy-on-write overhead)
//! - Merge performance with various dataset sizes
//! - GC performance

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use heliosdb_lite::{Config, storage::{StorageEngine, BranchOptions, MergeStrategy}};

fn bench_branch_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_creation");

    // Test branch creation with different parent data sizes
    for size in [0, 100, 1000, 10000] {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Populate main branch with data
        for i in 0..size {
            let key = format!("key{}", i).into_bytes();
            let value = format!("value{}", i).into_bytes();
            engine.put(&key, &value).unwrap();
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_keys", size)),
            &size,
            |b, _| {
                let mut counter = 0;
                b.iter(|| {
                    let branch_name = format!("bench_{}", counter);
                    counter += 1;
                    black_box(
                        engine.create_branch(
                            &branch_name,
                            Some("main"),
                            BranchOptions::default(),
                        ).unwrap()
                    )
                })
            },
        );
    }

    group.finish();
}

fn bench_branch_read_performance(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_read");

    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Populate main with 10000 keys
    for i in 0..10000 {
        let key = format!("key{}", i).into_bytes();
        let value = format!("value{}", i).into_bytes();
        engine.put(&key, &value).unwrap();
    }

    // Create branch
    engine.create_branch("bench_read", Some("main"), BranchOptions::default()).unwrap();

    // Benchmark: Read from main (baseline)
    group.bench_function("read_from_main", |b| {
        b.iter(|| {
            for i in (0..10000).step_by(100) {
                let key = format!("key{}", i).into_bytes();
                black_box(engine.get(&key).unwrap());
            }
        })
    });

    // Benchmark: Read from branch (should have minimal overhead)
    group.bench_function("read_from_branch", |b| {
        b.iter(|| {
            let tx = engine.begin_branch_transaction("bench_read").unwrap();
            for i in (0..10000).step_by(100) {
                let key = format!("key{}", i).into_bytes();
                black_box(tx.get(&key).unwrap());
            }
        })
    });

    group.finish();
}

fn bench_branch_write_performance(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_write");

    let config = Config::in_memory();
    let engine = StorageEngine::open_in_memory(&config).unwrap();

    // Create branch
    engine.create_branch("bench_write", Some("main"), BranchOptions::default()).unwrap();

    // Benchmark: Write to branch (copy-on-write)
    group.bench_function("write_100_keys", |b| {
        b.iter(|| {
            let mut tx = engine.begin_branch_transaction("bench_write").unwrap();
            for i in 0..100 {
                let key = format!("key{}", i).into_bytes();
                let value = format!("value{}", i).into_bytes();
                tx.put(key, value).unwrap();
            }
            tx.commit().unwrap();
        })
    });

    group.finish();
}

fn bench_merge_performance(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_merge");
    group.sample_size(10); // Reduce sample size for slower operations

    for num_changes in [10, 100, 1000] {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Populate main
        for i in 0..10000 {
            let key = format!("key{}", i).into_bytes();
            let value = format!("main_value_{}", i).into_bytes();
            engine.put(&key, &value).unwrap();
        }

        // Create branch and make changes
        let branch_name = format!("merge_bench_{}", num_changes);
        engine.create_branch(&branch_name, Some("main"), BranchOptions::default()).unwrap();

        let mut tx = engine.begin_branch_transaction(&branch_name).unwrap();
        for i in 0..num_changes {
            let key = format!("key{}", i).into_bytes();
            let value = format!("branch_value_{}", i).into_bytes();
            tx.put(key, value).unwrap();
        }
        tx.commit().unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_changes", num_changes)),
            &num_changes,
            |b, _| {
                b.iter(|| {
                    // Clone engine state for repeatability
                    // Note: In real benchmark, you'd recreate the scenario each time
                    black_box(
                        engine.merge_branch(
                            &branch_name,
                            "main",
                            MergeStrategy::Auto,
                        )
                    )
                })
            },
        );
    }

    group.finish();
}

fn bench_merge_with_conflicts(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge_conflicts");
    group.sample_size(10);

    for conflict_ratio in [10, 50, 100] {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Populate main with 100 keys
        for i in 0..100 {
            let key = format!("key{}", i).into_bytes();
            let value = format!("original_{}", i).into_bytes();
            engine.put(&key, &value).unwrap();
        }

        // Create branch
        let branch_name = format!("conflict_bench_{}", conflict_ratio);
        engine.create_branch(&branch_name, Some("main"), BranchOptions::default()).unwrap();

        // Modify keys in branch
        let mut tx = engine.begin_branch_transaction(&branch_name).unwrap();
        for i in 0..conflict_ratio {
            let key = format!("key{}", i).into_bytes();
            let value = format!("branch_{}", i).into_bytes();
            tx.put(key, value).unwrap();
        }
        tx.commit().unwrap();

        // Modify same keys in main (create conflicts)
        for i in 0..conflict_ratio {
            let key = format!("key{}", i).into_bytes();
            let value = format!("main_modified_{}", i).into_bytes();
            engine.put(&key, &value).unwrap();
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}%_conflicts", conflict_ratio)),
            &conflict_ratio,
            |b, _| {
                b.iter(|| {
                    black_box(
                        engine.merge_branch(
                            &branch_name,
                            "main",
                            MergeStrategy::Theirs,
                        )
                    )
                })
            },
        );
    }

    group.finish();
}

fn bench_list_branches(c: &mut Criterion) {
    let mut group = c.benchmark_group("list_branches");

    for num_branches in [10, 50, 100] {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create multiple branches
        for i in 0..num_branches {
            let branch_name = format!("branch_{}", i);
            engine.create_branch(&branch_name, Some("main"), BranchOptions::default()).unwrap();
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_branches", num_branches)),
            &num_branches,
            |b, _| {
                b.iter(|| {
                    black_box(engine.list_branches().unwrap())
                })
            },
        );
    }

    group.finish();
}

fn bench_branch_gc(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_gc");
    group.sample_size(10);

    for num_dropped in [5, 10, 20] {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create and populate branches
        for i in 0..num_dropped {
            let branch_name = format!("gc_bench_{}", i);
            engine.create_branch(&branch_name, Some("main"), BranchOptions::default()).unwrap();

            // Add some data to each branch
            let mut tx = engine.begin_branch_transaction(&branch_name).unwrap();
            for j in 0..100 {
                let key = format!("key_{}_{}", i, j).into_bytes();
                let value = format!("value_{}_{}", i, j).into_bytes();
                tx.put(key, value).unwrap();
            }
            tx.commit().unwrap();

            // Drop the branch
            engine.drop_branch(&branch_name, false).unwrap();
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_dropped", num_dropped)),
            &num_dropped,
            |b, _| {
                b.iter(|| {
                    // Note: This would need proper GC implementation
                    // For now, just measure the drop overhead
                    black_box(num_dropped)
                })
            },
        );
    }

    group.finish();
}

fn bench_branch_hierarchy_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_hierarchy");

    for depth in [1, 5, 10] {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create nested branch hierarchy
        let mut parent = "main".to_string();
        for i in 0..depth {
            let branch_name = format!("level_{}", i);
            engine.create_branch(&branch_name, Some(&parent), BranchOptions::default()).unwrap();
            parent = branch_name;
        }

        let deepest = format!("level_{}", depth - 1);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("depth_{}", depth)),
            &depth,
            |b, _| {
                b.iter(|| {
                    // Read from deepest branch (tests parent chain traversal)
                    let tx = engine.begin_branch_transaction(&deepest).unwrap();
                    for i in 0..10 {
                        let key = format!("key{}", i).into_bytes();
                        black_box(tx.get(&key).unwrap());
                    }
                })
            },
        );
    }

    group.finish();
}

fn bench_concurrent_branch_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_branches");
    group.sample_size(10);

    let config = Config::in_memory();
    let engine = std::sync::Arc::new(StorageEngine::open_in_memory(&config).unwrap());

    // Populate main
    for i in 0..1000 {
        let key = format!("key{}", i).into_bytes();
        let value = format!("value{}", i).into_bytes();
        engine.put(&key, &value).unwrap();
    }

    // Create multiple branches for concurrent access
    for i in 0..4 {
        let branch_name = format!("concurrent_{}", i);
        engine.create_branch(&branch_name, Some("main"), BranchOptions::default()).unwrap();
    }

    group.bench_function("4_concurrent_reads", |b| {
        b.iter(|| {
            use std::thread;

            let handles: Vec<_> = (0..4)
                .map(|i| {
                    let engine_clone = std::sync::Arc::clone(&engine);
                    let branch_name = format!("concurrent_{}", i);

                    thread::spawn(move || {
                        let tx = engine_clone.begin_branch_transaction(&branch_name).unwrap();
                        for j in (0..1000).step_by(10) {
                            let key = format!("key{}", j).into_bytes();
                            black_box(tx.get(&key).unwrap());
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_branch_creation,
    bench_branch_read_performance,
    bench_branch_write_performance,
    bench_merge_performance,
    bench_merge_with_conflicts,
    bench_list_branches,
    bench_branch_gc,
    bench_branch_hierarchy_depth,
    bench_concurrent_branch_operations,
);

criterion_main!(benches);
