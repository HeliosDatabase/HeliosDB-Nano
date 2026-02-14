//! Vector search benchmarks
//!
//! Benchmarks for HNSW index performance with different dataset sizes.

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use heliosdb_nano::vector::{HnswConfig, HnswIndex, DistanceMetric};

fn generate_random_vector(dim: usize, seed: u64) -> Vec<f32> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};

    let mut hasher = RandomState::new().build_hasher();
    seed.hash(&mut hasher);

    (0..dim)
        .map(|i| {
            let mut h = RandomState::new().build_hasher();
            (seed + i as u64).hash(&mut h);
            ((h.finish() % 10000) as f32 / 10000.0)
        })
        .collect()
}

fn benchmark_hnsw_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("hnsw_insert");

    for size in [100, 1000, 10000].iter() {
        let config = HnswConfig {
            dimension: 384,
            max_connections: 16,
            ef_construction: 200,
            distance_metric: DistanceMetric::L2,
        };

        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.iter(|| {
                let index = HnswIndex::new(config.clone()).unwrap();
                for i in 0..size {
                    let vector = generate_random_vector(384, i as u64);
                    index.insert(i as u64, &vector).unwrap();
                }
            });
        });
    }

    group.finish();
}

fn benchmark_hnsw_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("hnsw_search");

    for size in [1000, 10000, 100000].iter() {
        let config = HnswConfig {
            dimension: 384,
            max_connections: 16,
            ef_construction: 200,
            distance_metric: DistanceMetric::L2,
        };

        // Build index once
        let index = HnswIndex::new(config).unwrap();
        for i in 0..*size {
            let vector = generate_random_vector(384, i as u64);
            index.insert(i as u64, &vector).unwrap();
        }

        let query = generate_random_vector(384, 999999);

        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, _| {
            b.iter(|| {
                let results = index.search(black_box(&query), black_box(10)).unwrap();
                black_box(results);
            });
        });
    }

    group.finish();
}

fn benchmark_vector_distances(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_distances");

    for dim in [128, 384, 768, 1536].iter() {
        let v1 = generate_random_vector(*dim, 1);
        let v2 = generate_random_vector(*dim, 2);

        group.bench_with_input(
            BenchmarkId::new("l2", dim),
            dim,
            |b, _| {
                b.iter(|| {
                    let dist = heliosdb_nano::vector::l2_distance(
                        black_box(&v1),
                        black_box(&v2)
                    );
                    black_box(dist);
                });
            }
        );

        group.bench_with_input(
            BenchmarkId::new("cosine", dim),
            dim,
            |b, _| {
                b.iter(|| {
                    let dist = heliosdb_nano::vector::cosine_distance(
                        black_box(&v1),
                        black_box(&v2)
                    );
                    black_box(dist);
                });
            }
        );

        group.bench_with_input(
            BenchmarkId::new("inner_product", dim),
            dim,
            |b, _| {
                b.iter(|| {
                    let dist = heliosdb_nano::vector::inner_product_distance(
                        black_box(&v1),
                        black_box(&v2)
                    );
                    black_box(dist);
                });
            }
        );
    }

    group.finish();
}

fn benchmark_knn_accuracy(c: &mut Criterion) {
    let mut group = c.benchmark_group("knn_accuracy");

    // Test different k values
    for k in [1, 10, 50, 100].iter() {
        let config = HnswConfig {
            dimension: 128,
            max_connections: 16,
            ef_construction: 200,
            distance_metric: DistanceMetric::L2,
        };

        let index = HnswIndex::new(config).unwrap();
        for i in 0..10000 {
            let vector = generate_random_vector(128, i);
            index.insert(i, &vector).unwrap();
        }

        let query = generate_random_vector(128, 99999);

        group.bench_with_input(BenchmarkId::from_parameter(k), k, |b, &k| {
            b.iter(|| {
                let results = index.search(black_box(&query), black_box(k)).unwrap();
                black_box(results);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_hnsw_insert,
    benchmark_hnsw_search,
    benchmark_vector_distances,
    benchmark_knn_accuracy
);
criterion_main!(benches);
