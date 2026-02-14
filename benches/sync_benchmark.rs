//! Performance Benchmarks for HeliosDB-Lite v2.3.0 Sync Protocol
//!
//! This benchmark suite measures the performance of critical sync operations:
//! - Change log append operations
//! - Pull/push synchronization cycles
//! - Conflict detection and resolution
//! - Delta compression and application
//! - Large dataset synchronization
//! - Concurrent operations

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use heliosdb_nano::sync::{
    ChangeLogImpl, ChangeType, ConflictChangeEntry, ConflictChangeOperation, ConflictDetector,
    ConflictResolutionV2 as ConflictResolution, RowDelta, SyncClient, SyncConfig, SyncServer,
    VectorClock,
};
use rocksdb::{Options, DB};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use uuid::Uuid;

// ============================================================================
// Benchmark Setup Utilities
// ============================================================================

fn create_test_db() -> (Arc<DB>, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mut opts = Options::default();
    opts.create_if_missing(true);
    let db = DB::open(&opts, temp_dir.path()).expect("Failed to open DB");
    (Arc::new(db), temp_dir)
}

fn create_change_entry(
    node_id: Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    operation: ConflictChangeOperation,
) -> ConflictChangeEntry {
    let mut vc = VectorClock::new();
    vc.increment(node_id);

    ConflictChangeEntry {
        data: vec![1, 2, 3, 4, 5],
        timestamp,
        node_id,
        vector_clock: vc,
        operation,
    }
}

fn create_row_delta(table: &str, row_id: u64, data_size: usize) -> RowDelta {
    use chrono::Utc;
    use heliosdb_nano::sync::Operation;

    let mut delta = RowDelta {
        table: table.to_string(),
        operation: Operation::Insert,
        row_id: vec![row_id as u8],
        data: vec![0u8; data_size],
        vector_clock: VectorClock::new(),
        timestamp: Utc::now(),
        checksum: 0,
    };

    delta.checksum = delta.calculate_checksum();
    delta
}

// ============================================================================
// Change Log Benchmarks
// ============================================================================

fn bench_change_log_append(c: &mut Criterion) {
    let (db, _temp_dir) = create_test_db();
    let change_log = ChangeLogImpl::new(db).expect("Failed to create change log");

    c.bench_function("change_log_append_single", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            let change = ChangeType::Insert {
                table: "users".to_string(),
                row_id: counter,
                data: vec![0u8; 100],
            };

            let result = change_log.append(counter, change, VectorClock::new());
            counter += 1;
            black_box(result)
        });
    });
}

fn bench_change_log_append_batched(c: &mut Criterion) {
    let mut group = c.benchmark_group("change_log_append_batch");

    for batch_size in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*batch_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            batch_size,
            |b, &size| {
                let (db, _temp_dir) = create_test_db();
                let change_log = ChangeLogImpl::new(db).expect("Failed to create change log");

                b.iter(|| {
                    for i in 0..size {
                        let change = ChangeType::Insert {
                            table: "users".to_string(),
                            row_id: i as u64,
                            data: vec![0u8; 100],
                        };

                        change_log
                            .append(i as u64, change, VectorClock::new())
                            .expect("Append failed");
                    }
                });
            },
        );
    }
    group.finish();
}

fn bench_change_log_query(c: &mut Criterion) {
    let (db, _temp_dir) = create_test_db();
    let change_log = ChangeLogImpl::new(db).expect("Failed to create change log");

    // Populate with 1000 entries
    for i in 0..1000 {
        let change = ChangeType::Insert {
            table: format!("table_{}", i % 10),
            row_id: i,
            data: vec![0u8; 100],
        };

        change_log
            .append(i, change, VectorClock::new())
            .expect("Append failed");
    }

    c.bench_function("change_log_query_since_lsn", |b| {
        b.iter(|| {
            let entries = change_log.query_since_lsn(black_box(500));
            black_box(entries)
        });
    });

    c.bench_function("change_log_query_by_table", |b| {
        b.iter(|| {
            let entries = change_log.query_by_table(black_box("table_0"));
            black_box(entries)
        });
    });
}

fn bench_change_log_compaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("change_log_compact");

    for entry_count in [100, 1000, 10000].iter() {
        group.throughput(Throughput::Elements(*entry_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(entry_count),
            entry_count,
            |b, &count| {
                b.iter_batched(
                    || {
                        // Setup: Create change log with entries
                        let (db, _temp_dir) = create_test_db();
                        let change_log =
                            ChangeLogImpl::new(db).expect("Failed to create change log");

                        for i in 0..count {
                            let change = ChangeType::Insert {
                                table: "users".to_string(),
                                row_id: i as u64,
                                data: vec![0u8; 100],
                            };

                            change_log
                                .append(i as u64, change, VectorClock::new())
                                .expect("Append failed");
                        }

                        (change_log, _temp_dir)
                    },
                    |(change_log, _temp_dir)| {
                        // Benchmark: Compact half the entries
                        let watermark = count / 2;
                        change_log.compact(watermark as u64).expect("Compact failed")
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// ============================================================================
// Conflict Detection Benchmarks
// ============================================================================

fn bench_conflict_detection(c: &mut Criterion) {
    let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();

    let mut local = create_change_entry(node1, chrono::Utc::now(), ConflictChangeOperation::Update);
    let mut remote =
        create_change_entry(node2, chrono::Utc::now(), ConflictChangeOperation::Update);

    // Make them concurrent
    local.vector_clock.increment(node1);
    remote.vector_clock.increment(node2);

    c.bench_function("conflict_detect_simple", |b| {
        b.iter(|| {
            detector.detect(
                black_box("users"),
                black_box(&vec![1]),
                black_box(&local),
                black_box(&remote),
            )
        });
    });
}

fn bench_conflict_detection_large_vc(c: &mut Criterion) {
    let mut group = c.benchmark_group("conflict_detect_vector_clock");

    for vc_size in [10, 50, 100].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(vc_size), vc_size, |b, &size| {
            let detector = ConflictDetector::new(ConflictResolution::VectorClockCausal, Uuid::new_v4());

            let nodes: Vec<Uuid> = (0..size * 2).map(|_| Uuid::new_v4()).collect();

            let mut local = create_change_entry(nodes[0], chrono::Utc::now(), ConflictChangeOperation::Update);
            let mut remote = create_change_entry(nodes[1], chrono::Utc::now(), ConflictChangeOperation::Update);

            // Populate vector clocks
            for node in &nodes[0..size] {
                local.vector_clock.increment(*node);
            }
            for node in &nodes[size..size * 2] {
                remote.vector_clock.increment(*node);
            }

            b.iter(|| {
                detector.detect(
                    black_box("users"),
                    black_box(&vec![1]),
                    black_box(&local),
                    black_box(&remote),
                )
            });
        });
    }
    group.finish();
}

fn bench_conflict_resolution(c: &mut Criterion) {
    let mut group = c.benchmark_group("conflict_resolution");

    // Last-Write-Wins
    {
        let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let now = chrono::Utc::now();
        let local = create_change_entry(node1, now, ConflictChangeOperation::Update);
        let remote = create_change_entry(
            node2,
            now + chrono::Duration::seconds(1),
            ConflictChangeOperation::Update,
        );

        let mut local_concurrent = local.clone();
        let mut remote_concurrent = remote.clone();
        local_concurrent.vector_clock.increment(node1);
        remote_concurrent.vector_clock.increment(node2);

        let conflict = detector
            .detect("users", &vec![1], &local_concurrent, &remote_concurrent)
            .unwrap();

        group.bench_function("resolve_lww", |b| {
            b.iter(|| detector.resolve(black_box(conflict.clone())));
        });
    }

    // Vector Clock Causal
    {
        let detector =
            ConflictDetector::new(ConflictResolution::VectorClockCausal, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let now = chrono::Utc::now();
        let mut local = create_change_entry(node1, now, ConflictChangeOperation::Update);
        let mut remote = create_change_entry(node2, now, ConflictChangeOperation::Update);

        local.vector_clock.increment(node1);
        remote.vector_clock.increment(node2);

        let conflict = detector
            .detect("users", &vec![1], &local, &remote)
            .unwrap();

        group.bench_function("resolve_vector_clock", |b| {
            b.iter(|| detector.resolve(black_box(conflict.clone())));
        });
    }

    group.finish();
}

// ============================================================================
// Delta Operations Benchmarks
// ============================================================================

fn bench_delta_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_creation");

    for data_size in [100, 1024, 10240].iter() {
        group.throughput(Throughput::Bytes(*data_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(data_size),
            data_size,
            |b, &size| {
                b.iter(|| {
                    let delta = create_row_delta(black_box("users"), black_box(1), black_box(size));
                    black_box(delta)
                });
            },
        );
    }
    group.finish();
}

fn bench_delta_checksum(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_checksum");

    for data_size in [100, 1024, 10240].iter() {
        group.throughput(Throughput::Bytes(*data_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(data_size),
            data_size,
            |b, &size| {
                let delta = create_row_delta("users", 1, size);

                b.iter(|| {
                    let checksum = delta.calculate_checksum();
                    black_box(checksum)
                });
            },
        );
    }
    group.finish();
}

fn bench_delta_verification(c: &mut Criterion) {
    let delta = create_row_delta("users", 1, 1024);

    c.bench_function("delta_verify_checksum", |b| {
        b.iter(|| {
            let valid = delta.verify_checksum();
            black_box(valid)
        });
    });
}

// ============================================================================
// Vector Clock Benchmarks
// ============================================================================

fn bench_vector_clock_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_clock");

    let node_id = Uuid::new_v4();

    // Increment
    group.bench_function("increment", |b| {
        let mut vc = VectorClock::new();
        b.iter(|| {
            vc.increment(black_box(node_id));
        });
    });

    // Merge
    {
        let mut vc1 = VectorClock::new();
        let mut vc2 = VectorClock::new();
        vc1.increment(Uuid::new_v4());
        vc2.increment(Uuid::new_v4());

        group.bench_function("merge", |b| {
            let mut vc = vc1.clone();
            b.iter(|| {
                vc.merge(black_box(&vc2));
            });
        });
    }

    // Happens-before
    {
        let mut vc1 = VectorClock::new();
        let mut vc2 = VectorClock::new();
        vc1.increment(node_id);
        vc2.increment(node_id);
        vc2.increment(node_id);

        group.bench_function("happens_before", |b| {
            b.iter(|| {
                let result = vc1.happens_before(black_box(&vc2));
                black_box(result)
            });
        });
    }

    // Is concurrent
    {
        let mut vc1 = VectorClock::new();
        let mut vc2 = VectorClock::new();
        vc1.increment(Uuid::new_v4());
        vc2.increment(Uuid::new_v4());

        group.bench_function("is_concurrent", |b| {
            b.iter(|| {
                let result = vc1.is_concurrent(black_box(&vc2));
                black_box(result)
            });
        });
    }

    group.finish();
}

// ============================================================================
// Sync Client/Server Benchmarks
// ============================================================================

fn bench_client_enqueue(c: &mut Criterion) {
    let mut group = c.benchmark_group("client_enqueue");

    for batch_size in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*batch_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            batch_size,
            |b, &size| {
                b.iter_batched(
                    || {
                        // Setup: Create client
                        let config = SyncConfig {
                            server_url: "http://localhost:8080".to_string(),
                            client_id: Uuid::new_v4(),
                            sync_interval: Duration::from_secs(30),
                            retry_interval: Duration::from_secs(5),
                            max_batch_size: 1000,
                            enable_compression: true,
                            enable_e2e_encryption: false,
                        };

                        SyncClient::new(config).expect("Failed to create client")
                    },
                    |mut client| {
                        // Benchmark: Enqueue changes
                        for i in 0..size {
                            let delta = create_row_delta("users", i as u64, 100);
                            client.enqueue_change(delta).expect("Enqueue failed");
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// ============================================================================
// Large Dataset Benchmarks
// ============================================================================

fn bench_large_dataset_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_dataset");
    group.sample_size(10); // Reduce sample size for slow benchmarks

    for dataset_size in [1000, 5000, 10000].iter() {
        group.throughput(Throughput::Elements(*dataset_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(dataset_size),
            dataset_size,
            |b, &size| {
                b.iter_batched(
                    || {
                        // Setup: Create change log and populate
                        let (db, _temp_dir) = create_test_db();
                        let change_log =
                            ChangeLogImpl::new(db).expect("Failed to create change log");

                        for i in 0..size {
                            let change = ChangeType::Insert {
                                table: "users".to_string(),
                                row_id: i as u64,
                                data: vec![0u8; 100],
                            };

                            change_log
                                .append(i as u64, change, VectorClock::new())
                                .expect("Append failed");
                        }

                        (change_log, _temp_dir)
                    },
                    |(change_log, _temp_dir)| {
                        // Benchmark: Query all entries
                        let entries = change_log.query_since_lsn(0).expect("Query failed");
                        black_box(entries)
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// ============================================================================
// Serialization Benchmarks
// ============================================================================

fn bench_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("serialization");

    let change = ChangeType::Insert {
        table: "users".to_string(),
        row_id: 42,
        data: vec![0u8; 1024],
    };

    let change_log_entry = heliosdb_nano::sync::ChangeEntry::new(
        100,
        1,
        change.clone(),
        VectorClock::new(),
    );

    // Serialize
    group.bench_function("serialize_change_entry", |b| {
        b.iter(|| {
            let bytes = change_log_entry.serialize();
            black_box(bytes)
        });
    });

    // Deserialize
    let serialized = change_log_entry.serialize().expect("Serialization failed");
    group.bench_function("deserialize_change_entry", |b| {
        b.iter(|| {
            let entry = heliosdb_nano::sync::ChangeEntry::deserialize(black_box(&serialized));
            black_box(entry)
        });
    });

    group.finish();
}

// ============================================================================
// Criterion Configuration
// ============================================================================

criterion_group!(
    change_log_benches,
    bench_change_log_append,
    bench_change_log_append_batched,
    bench_change_log_query,
    bench_change_log_compaction
);

criterion_group!(
    conflict_benches,
    bench_conflict_detection,
    bench_conflict_detection_large_vc,
    bench_conflict_resolution
);

criterion_group!(
    delta_benches,
    bench_delta_creation,
    bench_delta_checksum,
    bench_delta_verification
);

criterion_group!(
    vector_clock_benches,
    bench_vector_clock_operations
);

criterion_group!(
    client_benches,
    bench_client_enqueue
);

criterion_group!(
    large_dataset_benches,
    bench_large_dataset_operations
);

criterion_group!(
    serialization_benches,
    bench_serialization
);

criterion_main!(
    change_log_benches,
    conflict_benches,
    delta_benches,
    vector_clock_benches,
    client_benches,
    large_dataset_benches,
    serialization_benches
);
