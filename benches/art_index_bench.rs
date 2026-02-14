//! ART (Adaptive Radix Tree) Index Benchmarks
//!
//! Measures performance of ART index operations including:
//! - Point lookups (O(k) where k is key length)
//! - Insertions
//! - Deletions
//! - Range scans
//! - Prefix scans
//!
//! Performance targets:
//! - Point lookup: < 1μs
//! - Insert: O(k)
//! - Range scan: O(k + m) where m is result count

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use heliosdb_nano::storage::art_index::AdaptiveRadixTree;
use heliosdb_nano::storage::ArtIndexType;
use rand::Rng;

/// Generate random string key of given length
fn random_key(len: usize) -> Vec<u8> {
    let mut rng = rand::thread_rng();
    (0..len).map(|_| rng.gen_range(b'a'..=b'z')).collect()
}

/// Generate sequential numeric keys (big-endian for correct ordering)
fn sequential_keys(count: usize) -> Vec<Vec<u8>> {
    (0..count as u64)
        .map(|i| i.to_be_bytes().to_vec())
        .collect()
}

/// Generate random string keys of given count and length
fn random_keys(count: usize, len: usize) -> Vec<Vec<u8>> {
    (0..count).map(|_| random_key(len)).collect()
}

/// Benchmark point lookups
fn bench_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("art_lookup");

    for size in [100, 1_000, 10_000, 100_000] {
        // Setup: create index with N keys
        let mut art = AdaptiveRadixTree::new(
            "bench_idx",
            "bench_table",
            vec!["key".to_string()],
            ArtIndexType::PrimaryKey,
        );

        let keys = sequential_keys(size);
        for (i, key) in keys.iter().enumerate() {
            art.insert(key, i as u64).unwrap();
        }

        // Benchmark lookup of random existing keys
        let lookup_keys: Vec<_> = (0..1000)
            .map(|_| {
                let idx = rand::thread_rng().gen_range(0..size);
                keys[idx].clone()
            })
            .collect();

        group.throughput(Throughput::Elements(1000));
        group.bench_with_input(BenchmarkId::new("point_lookup", size), &size, |bench, _| {
            bench.iter(|| {
                for key in &lookup_keys {
                    black_box(art.get(key));
                }
            });
        });
    }

    group.finish();
}

/// Benchmark insertions
fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("art_insert");

    for size in [100, 1_000, 10_000] {
        let keys = sequential_keys(size);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("sequential_insert", size), &size, |bench, _| {
            bench.iter(|| {
                let mut art = AdaptiveRadixTree::new(
                    "bench_idx",
                    "bench_table",
                    vec!["key".to_string()],
                    ArtIndexType::PrimaryKey,
                );
                for (i, key) in keys.iter().enumerate() {
                    black_box(art.insert(key, i as u64).unwrap());
                }
            });
        });
    }

    group.finish();
}

/// Benchmark random insertions (more realistic workload)
fn bench_random_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("art_random_insert");

    for size in [100, 1_000, 10_000] {
        let keys = random_keys(size, 16); // 16-byte random keys

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("random_insert", size), &size, |bench, _| {
            bench.iter(|| {
                let mut art = AdaptiveRadixTree::new(
                    "bench_idx",
                    "bench_table",
                    vec!["key".to_string()],
                    ArtIndexType::PrimaryKey,
                );
                for (i, key) in keys.iter().enumerate() {
                    black_box(art.insert(key, i as u64).unwrap());
                }
            });
        });
    }

    group.finish();
}

/// Benchmark deletions
fn bench_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("art_delete");

    for size in [100, 1_000, 10_000] {
        let keys = sequential_keys(size);

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("delete", size), &size, |bench, _| {
            bench.iter_batched(
                || {
                    // Setup: create full index
                    let mut art = AdaptiveRadixTree::new(
                        "bench_idx",
                        "bench_table",
                        vec!["key".to_string()],
                        ArtIndexType::PrimaryKey,
                    );
                    for (i, key) in keys.iter().enumerate() {
                        art.insert(key, i as u64).unwrap();
                    }
                    art
                },
                |mut art| {
                    // Delete all keys
                    for key in &keys {
                        black_box(art.remove(key));
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Benchmark iteration (full scan)
fn bench_iteration(c: &mut Criterion) {
    let mut group = c.benchmark_group("art_iteration");

    for size in [100, 1_000, 10_000, 100_000] {
        // Setup: create index with N keys
        let mut art = AdaptiveRadixTree::new(
            "bench_idx",
            "bench_table",
            vec!["key".to_string()],
            ArtIndexType::PrimaryKey,
        );

        let keys = sequential_keys(size);
        for (i, key) in keys.iter().enumerate() {
            art.insert(key, i as u64).unwrap();
        }

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("full_scan", size), &size, |bench, _| {
            bench.iter(|| {
                let count: u64 = art.iter().map(|(_, v)| black_box(v)).count() as u64;
                count
            });
        });
    }

    group.finish();
}

/// Benchmark range scan
fn bench_range_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("art_range_scan");

    // Setup: create large index
    let size = 100_000;
    let mut art = AdaptiveRadixTree::new(
        "bench_idx",
        "bench_table",
        vec!["key".to_string()],
        ArtIndexType::PrimaryKey,
    );

    let keys = sequential_keys(size);
    for (i, key) in keys.iter().enumerate() {
        art.insert(key, i as u64).unwrap();
    }

    // Benchmark ranges of different sizes
    for range_size in [10, 100, 1_000, 10_000] {
        let start_idx = size / 4;
        let end_idx = start_idx + range_size;
        let start_key = &keys[start_idx];
        let end_key = &keys[end_idx];

        group.throughput(Throughput::Elements(range_size as u64));
        group.bench_with_input(BenchmarkId::new("range", range_size), &range_size, |bench, _| {
            bench.iter(|| {
                let count: usize = art.range(start_key, end_key).count();
                black_box(count)
            });
        });
    }

    group.finish();
}

/// Benchmark prefix scan
fn bench_prefix_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("art_prefix_scan");

    // Setup: create index with hierarchical keys
    let mut art = AdaptiveRadixTree::new(
        "bench_idx",
        "bench_table",
        vec!["path".to_string()],
        ArtIndexType::Manual,
    );

    // Insert keys like /region1/tenant1/user1, /region1/tenant1/user2, etc.
    let mut row_id = 0u64;
    for region in 1..=10 {
        for tenant in 1..=100 {
            for user in 1..=100 {
                let key = format!("/region{}/tenant{}/user{}", region, tenant, user);
                art.insert(key.as_bytes(), row_id).unwrap();
                row_id += 1;
            }
        }
    }

    // Benchmark different prefix selectivities
    // /region1/ matches 10,000 keys
    group.bench_function("prefix_10k", |bench| {
        bench.iter(|| {
            let count: usize = art.prefix_scan(b"/region1/").count();
            black_box(count)
        });
    });

    // /region1/tenant1/ matches 100 keys
    group.bench_function("prefix_100", |bench| {
        bench.iter(|| {
            let count: usize = art.prefix_scan(b"/region1/tenant1/").count();
            black_box(count)
        });
    });

    // /region1/tenant1/user1 matches 1 key
    group.bench_function("prefix_1", |bench| {
        bench.iter(|| {
            let count: usize = art.prefix_scan(b"/region1/tenant1/user1").count();
            black_box(count)
        });
    });

    group.finish();
}

/// Benchmark key encoding for different value types
fn bench_key_encoding(c: &mut Criterion) {
    use heliosdb_nano::storage::ArtIndexManager;
    use heliosdb_nano::Value;

    let mut group = c.benchmark_group("art_key_encoding");

    // Single integer
    let int_values = vec![Value::Int4(42)];
    group.bench_function("encode_int4", |bench| {
        bench.iter(|| {
            black_box(ArtIndexManager::encode_key(&int_values))
        });
    });

    // Single bigint
    let bigint_values = vec![Value::Int8(9223372036854775807i64)];
    group.bench_function("encode_int8", |bench| {
        bench.iter(|| {
            black_box(ArtIndexManager::encode_key(&bigint_values))
        });
    });

    // Single string
    let text_values = vec![Value::String("hello world".to_string())];
    group.bench_function("encode_text", |bench| {
        bench.iter(|| {
            black_box(ArtIndexManager::encode_key(&text_values))
        });
    });

    // Composite key (int, text)
    let composite_values = vec![
        Value::Int4(123),
        Value::String("user@example.com".to_string()),
    ];
    group.bench_function("encode_composite", |bench| {
        bench.iter(|| {
            black_box(ArtIndexManager::encode_key(&composite_values))
        });
    });

    group.finish();
}

/// Benchmark vs HashMap (for comparison)
fn bench_vs_hashmap(c: &mut Criterion) {
    use std::collections::HashMap;

    let mut group = c.benchmark_group("art_vs_hashmap");

    let size = 100_000;
    let keys = sequential_keys(size);

    // Setup ART
    let mut art = AdaptiveRadixTree::new(
        "bench_idx",
        "bench_table",
        vec!["key".to_string()],
        ArtIndexType::PrimaryKey,
    );
    for (i, key) in keys.iter().enumerate() {
        art.insert(key, i as u64).unwrap();
    }

    // Setup HashMap
    let mut hashmap: HashMap<Vec<u8>, u64> = HashMap::new();
    for (i, key) in keys.iter().enumerate() {
        hashmap.insert(key.clone(), i as u64);
    }

    // Random lookup keys
    let lookup_keys: Vec<_> = (0..1000)
        .map(|_| {
            let idx = rand::thread_rng().gen_range(0..size);
            keys[idx].clone()
        })
        .collect();

    group.throughput(Throughput::Elements(1000));

    group.bench_function("art_lookup", |bench| {
        bench.iter(|| {
            for key in &lookup_keys {
                black_box(art.get(key));
            }
        });
    });

    group.bench_function("hashmap_lookup", |bench| {
        bench.iter(|| {
            for key in &lookup_keys {
                black_box(hashmap.get(key));
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_lookup,
    bench_insert,
    bench_random_insert,
    bench_delete,
    bench_iteration,
    bench_range_scan,
    bench_prefix_scan,
    bench_key_encoding,
    bench_vs_hashmap,
);

criterion_main!(benches);
