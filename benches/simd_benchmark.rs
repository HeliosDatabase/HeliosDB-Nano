//! SIMD Performance Benchmarks
//!
//! Measures speedup of SIMD-accelerated vector operations compared to scalar baseline.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use heliosdb_nano::vector::simd;
use rand::Rng;

/// Generate random vector of given size
fn random_vector(size: usize) -> Vec<f32> {
    let mut rng = rand::thread_rng();
    (0..size).map(|_| rng.gen_range(-1.0..1.0)).collect()
}

/// Benchmark L2 distance at various dimensions
fn bench_l2_distance(c: &mut Criterion) {
    let mut group = c.benchmark_group("l2_distance");

    for size in [8, 16, 32, 64, 128, 256, 384, 512, 768, 1024, 1536] {
        let a = random_vector(size);
        let b = random_vector(size);

        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("simd", size), &size, |bench, _| {
            bench.iter(|| {
                simd::l2_distance(black_box(&a), black_box(&b))
            });
        });
    }

    group.finish();
}

/// Benchmark L2 distance squared (no sqrt)
fn bench_l2_distance_squared(c: &mut Criterion) {
    let mut group = c.benchmark_group("l2_distance_squared");

    for size in [64, 128, 256, 512, 768, 1024] {
        let a = random_vector(size);
        let b = random_vector(size);

        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("simd", size), &size, |bench, _| {
            bench.iter(|| {
                simd::l2_distance_squared(black_box(&a), black_box(&b))
            });
        });
    }

    group.finish();
}

/// Benchmark cosine distance at various dimensions
fn bench_cosine_distance(c: &mut Criterion) {
    let mut group = c.benchmark_group("cosine_distance");

    for size in [8, 16, 32, 64, 128, 256, 384, 512, 768, 1024, 1536] {
        let a = random_vector(size);
        let b = random_vector(size);

        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("simd", size), &size, |bench, _| {
            bench.iter(|| {
                simd::cosine_distance(black_box(&a), black_box(&b))
            });
        });
    }

    group.finish();
}

/// Benchmark dot product at various dimensions
fn bench_dot_product(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product");

    for size in [8, 16, 32, 64, 128, 256, 384, 512, 768, 1024, 1536] {
        let a = random_vector(size);
        let b = random_vector(size);

        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("simd", size), &size, |bench, _| {
            bench.iter(|| {
                simd::dot_product(black_box(&a), black_box(&b))
            });
        });
    }

    group.finish();
}

/// Benchmark realistic workload: OpenAI embedding dimensions
fn bench_openai_embeddings(c: &mut Criterion) {
    let mut group = c.benchmark_group("openai_embeddings");

    // Common OpenAI embedding dimensions
    let dims = [
        ("ada-002", 1536),
        ("text-embedding-3-small", 512),
        ("text-embedding-3-large", 3072),
    ];

    for (name, dim) in dims {
        let a = random_vector(dim);
        let b = random_vector(dim);

        group.throughput(Throughput::Elements(dim as u64));

        group.bench_with_input(BenchmarkId::new("l2", name), &dim, |bench, _| {
            bench.iter(|| {
                simd::l2_distance(black_box(&a), black_box(&b))
            });
        });

        group.bench_with_input(BenchmarkId::new("cosine", name), &dim, |bench, _| {
            bench.iter(|| {
                simd::cosine_distance(black_box(&a), black_box(&b))
            });
        });

        group.bench_with_input(BenchmarkId::new("dot", name), &dim, |bench, _| {
            bench.iter(|| {
                simd::dot_product(black_box(&a), black_box(&b))
            });
        });
    }

    group.finish();
}

/// Benchmark batch operations (simulating search workload)
fn bench_batch_distances(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_distances");

    let query = random_vector(768);
    let database: Vec<Vec<f32>> = (0..1000).map(|_| random_vector(768)).collect();

    group.throughput(Throughput::Elements(1000));

    group.bench_function("l2_1000x768", |bench| {
        bench.iter(|| {
            let mut distances = Vec::with_capacity(1000);
            for vec in &database {
                distances.push(simd::l2_distance(black_box(&query), black_box(vec)));
            }
            black_box(distances)
        });
    });

    group.bench_function("cosine_1000x768", |bench| {
        bench.iter(|| {
            let mut distances = Vec::with_capacity(1000);
            for vec in &database {
                distances.push(simd::cosine_distance(black_box(&query), black_box(vec)));
            }
            black_box(distances)
        });
    });

    group.finish();
}

/// Benchmark Product Quantization distance computation
fn bench_pq_distance(c: &mut Criterion) {
    use heliosdb_nano::vector::simd::quantization;

    let mut group = c.benchmark_group("pq_asymmetric_distance");

    for num_subquantizers in [8, 16, 32, 64] {
        let codebook_size = 256;

        let codes: Vec<u8> = (0..num_subquantizers).map(|i| (i * 17) as u8).collect();
        let distance_table: Vec<f32> = (0..num_subquantizers * codebook_size)
            .map(|i| (i as f32) * 0.01)
            .collect();

        group.throughput(Throughput::Elements(num_subquantizers as u64));

        group.bench_with_input(
            BenchmarkId::new("simd", num_subquantizers),
            &num_subquantizers,
            |bench, &m| {
                bench.iter(|| {
                    quantization::asymmetric_distance_simd(
                        black_box(&[]),
                        black_box(&codes),
                        black_box(&distance_table),
                        m,
                        codebook_size,
                    )
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_l2_distance,
    bench_l2_distance_squared,
    bench_cosine_distance,
    bench_dot_product,
    bench_openai_embeddings,
    bench_batch_distances,
    bench_pq_distance,
);

criterion_main!(benches);
