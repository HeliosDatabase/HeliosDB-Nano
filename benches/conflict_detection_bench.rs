use criterion::{black_box, criterion_group, criterion_main, Criterion};
use heliosdb_lite::sync::{
    ConflictChangeEntry as ChangeEntry, ConflictChangeOperation as ChangeOperation,
    ConflictDetector, ConflictResolutionV2 as ConflictResolution, VectorClock,
};
use uuid::Uuid;

fn create_test_entry(
    node_id: Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    operation: ChangeOperation,
) -> ChangeEntry {
    let mut vc = VectorClock::new();
    vc.increment(node_id);

    ChangeEntry {
        data: vec![1, 2, 3],
        timestamp,
        node_id,
        vector_clock: vc,
        operation,
    }
}

fn conflict_detection_benchmark(c: &mut Criterion) {
    let detector = ConflictDetector::new(ConflictResolution::VectorClockCausal, Uuid::new_v4());
    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();

    let mut local = create_test_entry(node1, chrono::Utc::now(), ChangeOperation::Update);
    let mut remote = create_test_entry(node2, chrono::Utc::now(), ChangeOperation::Update);

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
        })
    });
}

fn conflict_resolution_benchmark(c: &mut Criterion) {
    let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();

    let now = chrono::Utc::now();
    let local = create_test_entry(node1, now, ChangeOperation::Update);
    let remote =
        create_test_entry(node2, now + chrono::Duration::seconds(1), ChangeOperation::Update);

    let mut local_concurrent = local.clone();
    let mut remote_concurrent = remote.clone();
    local_concurrent.vector_clock.increment(node1);
    remote_concurrent.vector_clock.increment(node2);

    let conflict = detector
        .detect("users", &vec![1], &local_concurrent, &remote_concurrent)
        .unwrap();

    c.bench_function("conflict_resolve_lww", |b| {
        b.iter(|| detector.resolve(black_box(conflict.clone())))
    });
}

fn conflict_detection_large_vector_clock(c: &mut Criterion) {
    let detector = ConflictDetector::new(ConflictResolution::VectorClockCausal, Uuid::new_v4());

    // Create entries with large vector clocks (100 nodes)
    let nodes: Vec<Uuid> = (0..100).map(|_| Uuid::new_v4()).collect();

    let mut local = create_test_entry(nodes[0], chrono::Utc::now(), ChangeOperation::Update);
    let mut remote = create_test_entry(nodes[1], chrono::Utc::now(), ChangeOperation::Update);

    // Populate vector clocks with many nodes
    for node in &nodes[0..50] {
        local.vector_clock.increment(*node);
    }
    for node in &nodes[50..100] {
        remote.vector_clock.increment(*node);
    }

    c.bench_function("conflict_detect_large_vc", |b| {
        b.iter(|| {
            detector.detect(
                black_box("users"),
                black_box(&vec![1]),
                black_box(&local),
                black_box(&remote),
            )
        })
    });
}

criterion_group!(
    benches,
    conflict_detection_benchmark,
    conflict_resolution_benchmark,
    conflict_detection_large_vector_clock
);
criterion_main!(benches);
