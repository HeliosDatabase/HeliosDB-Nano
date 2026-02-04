// Phase 3 Feature Benchmarks

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use rand::Rng;

// ==================== Product Quantization Benchmarks ====================

fn generate_random_vector(dim: usize) -> Vec<f32> {
    let mut rng = rand::thread_rng();
    (0..dim).map(|_| rng.gen::<f32>()).collect()
}

fn generate_random_vectors(n: usize, dim: usize) -> Vec<Vec<f32>> {
    (0..n).map(|_| generate_random_vector(dim)).collect()
}

// Placeholder PQ implementation for benchmarking
struct ProductQuantizer {
    dim: usize,
    num_subquantizers: usize,
    k: usize,
    codebooks: Vec<Vec<Vec<f32>>>,
    distance_tables: Vec<Vec<f32>>,
}

impl ProductQuantizer {
    fn new(dim: usize, num_subquantizers: usize, k: usize) -> Self {
        Self {
            dim,
            num_subquantizers,
            k,
            codebooks: vec![vec![vec![0.0; dim / num_subquantizers]; k]; num_subquantizers],
            distance_tables: Vec::new(),
        }
    }

    fn train(&mut self, vectors: &[Vec<f32>]) {
        // Simplified training
        let subdim = self.dim / self.num_subquantizers;
        let mut rng = rand::thread_rng();
        for m in 0..self.num_subquantizers {
            for k in 0..self.k {
                let idx = rng.gen_range(0..vectors.len());
                let start = m * subdim;
                let end = start + subdim;
                self.codebooks[m][k] = vectors[idx][start..end].to_vec();
            }
        }
    }

    fn encode(&self, vector: &[f32]) -> Vec<u8> {
        let subdim = self.dim / self.num_subquantizers;
        (0..self.num_subquantizers)
            .map(|m| {
                let start = m * subdim;
                let end = start + subdim;
                let subvector = &vector[start..end];
                self.find_nearest_centroid(subvector, &self.codebooks[m]) as u8
            })
            .collect()
    }

    fn precompute_distance_table(&mut self, query: &[f32]) {
        let subdim = self.dim / self.num_subquantizers;
        self.distance_tables = (0..self.num_subquantizers)
            .map(|m| {
                let start = m * subdim;
                let end = start + subdim;
                let query_subvector = &query[start..end];
                self.codebooks[m]
                    .iter()
                    .map(|centroid| euclidean_distance_squared(query_subvector, centroid))
                    .collect()
            })
            .collect();
    }

    fn compute_distance_with_table(&self, codes: &[u8]) -> f32 {
        codes
            .iter()
            .enumerate()
            .map(|(m, &code)| self.distance_tables[m][code as usize])
            .sum::<f32>()
            .sqrt()
    }

    fn find_nearest_centroid(&self, vector: &[f32], centroids: &[Vec<f32>]) -> usize {
        centroids
            .iter()
            .enumerate()
            .min_by(|(_, c1), (_, c2)| {
                let d1 = euclidean_distance_squared(vector, c1);
                let d2 = euclidean_distance_squared(vector, c2);
                d1.partial_cmp(&d2).unwrap()
            })
            .map(|(i, _)| i)
            .unwrap()
    }
}

fn euclidean_distance_squared(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y).powi(2)).sum()
}

fn bench_pq_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("pq_encoding");

    let dim = 768;
    let num_subquantizers = 8;
    let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);

    let training_vectors = generate_random_vectors(5000, dim);
    pq.train(&training_vectors);

    let vector = generate_random_vector(dim);

    group.throughput(Throughput::Elements(1));
    group.bench_function("encode_768d_8sub", |b| {
        b.iter(|| {
            let encoded = pq.encode(black_box(&vector));
            black_box(encoded);
        });
    });

    group.finish();
}

fn bench_pq_distance_computation(c: &mut Criterion) {
    let mut group = c.benchmark_group("pq_distance");

    let dim = 768;
    let num_subquantizers = 8;
    let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);

    let training_vectors = generate_random_vectors(5000, dim);
    pq.train(&training_vectors);

    let query = generate_random_vector(dim);
    pq.precompute_distance_table(&query);

    let test_vector = generate_random_vector(dim);
    let codes = pq.encode(&test_vector);

    group.throughput(Throughput::Elements(1));
    group.bench_function("distance_with_table", |b| {
        b.iter(|| {
            let dist = pq.compute_distance_with_table(black_box(&codes));
            black_box(dist);
        });
    });

    group.finish();
}

fn bench_pq_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("pq_search");

    let dim = 768;
    let num_subquantizers = 8;

    for num_vectors in [10_000, 100_000, 1_000_000].iter() {
        let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);
        let training_vectors = generate_random_vectors(5000, dim);
        pq.train(&training_vectors);

        // Generate database vectors and encode them
        let database_vectors = generate_random_vectors(*num_vectors, dim);
        let encoded_database: Vec<Vec<u8>> = database_vectors
            .iter()
            .map(|v| pq.encode(v))
            .collect();

        let query = generate_random_vector(dim);
        pq.precompute_distance_table(&query);

        group.throughput(Throughput::Elements(*num_vectors as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_vectors),
            num_vectors,
            |b, _| {
                b.iter(|| {
                    // Find top-10 nearest neighbors
                    let mut distances: Vec<(usize, f32)> = encoded_database
                        .iter()
                        .enumerate()
                        .map(|(i, codes)| (i, pq.compute_distance_with_table(codes)))
                        .collect();

                    distances.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                    let top10 = &distances[..10.min(distances.len())];
                    black_box(top10);
                });
            },
        );
    }

    group.finish();
}

fn bench_pq_batch_distance(c: &mut Criterion) {
    let mut group = c.benchmark_group("pq_batch_distance");

    let dim = 768;
    let num_subquantizers = 8;
    let mut pq = ProductQuantizer::new(dim, num_subquantizers, 256);

    let training_vectors = generate_random_vectors(5000, dim);
    pq.train(&training_vectors);

    let query = generate_random_vector(dim);
    pq.precompute_distance_table(&query);

    for batch_size in [100, 1000, 10000].iter() {
        let vectors = generate_random_vectors(*batch_size, dim);
        let codes_batch: Vec<Vec<u8>> = vectors.iter().map(|v| pq.encode(v)).collect();

        group.throughput(Throughput::Elements(*batch_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            batch_size,
            |b, _| {
                b.iter(|| {
                    let distances: Vec<f32> = codes_batch
                        .iter()
                        .map(|codes| pq.compute_distance_with_table(codes))
                        .collect();
                    black_box(distances);
                });
            },
        );
    }

    group.finish();
}

// ==================== Compression Benchmarks ====================

fn bench_fsst_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("fsst_compression");

    // Simulate FSST compression
    fn fsst_compress_simulation(data: &[u8]) -> Vec<u8> {
        // Simplified: just apply zstd as placeholder
        zstd::encode_all(data, 3).unwrap()
    }

    fn fsst_decompress_simulation(data: &[u8]) -> Vec<u8> {
        zstd::decode_all(data).unwrap()
    }

    for size_kb in [1, 10, 100, 1000].iter() {
        let size_bytes = size_kb * 1024;
        let data: Vec<u8> = (0..size_bytes)
            .map(|i| (b"SELECT * FROM users WHERE user_id = 123; "[i % 43]) as u8)
            .collect();

        group.throughput(Throughput::Bytes(size_bytes as u64));

        group.bench_with_input(
            BenchmarkId::new("compress", size_kb),
            &size_kb,
            |b, _| {
                b.iter(|| {
                    let compressed = fsst_compress_simulation(black_box(&data));
                    black_box(compressed);
                });
            },
        );

        let compressed = fsst_compress_simulation(&data);

        group.bench_with_input(
            BenchmarkId::new("decompress", size_kb),
            &size_kb,
            |b, _| {
                b.iter(|| {
                    let decompressed = fsst_decompress_simulation(black_box(&compressed));
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

fn bench_alp_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("alp_compression");

    // Simulate ALP compression for floats
    fn alp_compress_simulation(floats: &[f64]) -> Vec<u8> {
        // Simplified: convert to bytes and compress
        let bytes: Vec<u8> = floats
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        zstd::encode_all(&bytes[..], 3).unwrap()
    }

    for num_floats in [1000, 10000, 100000].iter() {
        let floats: Vec<f64> = (0..*num_floats)
            .map(|i| 100.0 + (i as f64) * 0.1)
            .collect();

        let size_bytes = num_floats * 8;
        group.throughput(Throughput::Bytes(size_bytes as u64));

        group.bench_with_input(
            BenchmarkId::from_parameter(num_floats),
            num_floats,
            |b, _| {
                b.iter(|| {
                    let compressed = alp_compress_simulation(black_box(&floats));
                    black_box(compressed);
                });
            },
        );
    }

    group.finish();
}

// ==================== Incremental MV Benchmarks ====================

fn bench_incremental_mv_refresh(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_mv_refresh");

    // Simulate incremental refresh
    fn simulate_refresh(base_rows: usize, delta_rows: usize) -> usize {
        // Simplified: just aggregate delta rows
        let mut sum = 0;
        for _ in 0..delta_rows {
            sum += 1;
        }
        sum
    }

    for base_rows in [10_000, 100_000, 1_000_000].iter() {
        let delta_rows = 100; // Small delta

        group.throughput(Throughput::Elements(delta_rows as u64));
        group.bench_with_input(
            BenchmarkId::new("refresh", format!("base_{}", base_rows)),
            base_rows,
            |b, _| {
                b.iter(|| {
                    let result = simulate_refresh(black_box(*base_rows), black_box(delta_rows));
                    black_box(result);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_pq_encoding,
    bench_pq_distance_computation,
    bench_pq_search,
    bench_pq_batch_distance,
    bench_fsst_compression,
    bench_alp_compression,
    bench_incremental_mv_refresh,
);
criterion_main!(benches);
