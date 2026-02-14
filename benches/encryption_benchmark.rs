//! Encryption performance benchmarks
//!
//! Measures the performance overhead of transparent data encryption.

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use heliosdb_nano::{Config, Column, DataType, Schema, Tuple, Value};

fn setup_encrypted_storage() -> heliosdb_nano::storage::StorageEngine {
    let hex_key = generate_random_hex_key();
    std::env::set_var("BENCH_ENCRYPTION_KEY", &hex_key);

    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_nano::config::KeySource::Environment("BENCH_ENCRYPTION_KEY".to_string());

    heliosdb_nano::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage")
}

fn setup_unencrypted_storage() -> heliosdb_nano::storage::StorageEngine {
    let config = Config::in_memory();
    heliosdb_nano::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open unencrypted storage")
}

fn generate_random_hex_key() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let key: [u8; 32] = rng.gen();
    hex::encode(key)
}

fn benchmark_put_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("put_get");

    // Test with different payload sizes
    for size in [64, 256, 1024, 4096, 16384].iter() {
        let payload = vec![0u8; *size];

        group.throughput(Throughput::Bytes(*size as u64));

        // Encrypted storage
        group.bench_with_input(
            BenchmarkId::new("encrypted", size),
            size,
            |b, _| {
                let storage = setup_encrypted_storage();
                let key = b"benchmark_key".to_vec();

                b.iter(|| {
                    storage.put(&key, black_box(&payload)).unwrap();
                    let _result = storage.get(&key).unwrap();
                });
            },
        );

        // Unencrypted storage
        group.bench_with_input(
            BenchmarkId::new("unencrypted", size),
            size,
            |b, _| {
                let storage = setup_unencrypted_storage();
                let key = b"benchmark_key".to_vec();

                b.iter(|| {
                    storage.put(&key, black_box(&payload)).unwrap();
                    let _result = storage.get(&key).unwrap();
                });
            },
        );
    }

    group.finish();
}

fn benchmark_insert_tuple(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_tuple");

    // Encrypted storage
    group.bench_function("encrypted", |b| {
        let storage = setup_encrypted_storage();
        let catalog = storage.catalog();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
            Column::new("email", DataType::Text),
            Column::new("age", DataType::Int4),
        ]);

        catalog.create_table("bench_users", schema)
            .expect("Failed to create table");

        let mut counter = 0i32;

        b.iter(|| {
            let tuple = Tuple::new(vec![
                Value::Int4(counter),
                Value::String(format!("user_{}", counter)),
                Value::String(format!("user_{}@example.com", counter)),
                Value::Int4(25 + (counter % 50)),
            ]);

            storage.insert_tuple("bench_users", black_box(tuple)).unwrap();
            counter += 1;
        });
    });

    // Unencrypted storage
    group.bench_function("unencrypted", |b| {
        let storage = setup_unencrypted_storage();
        let catalog = storage.catalog();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
            Column::new("email", DataType::Text),
            Column::new("age", DataType::Int4),
        ]);

        catalog.create_table("bench_users", schema)
            .expect("Failed to create table");

        let mut counter = 0i32;

        b.iter(|| {
            let tuple = Tuple::new(vec![
                Value::Int4(counter),
                Value::String(format!("user_{}", counter)),
                Value::String(format!("user_{}@example.com", counter)),
                Value::Int4(25 + (counter % 50)),
            ]);

            storage.insert_tuple("bench_users", black_box(tuple)).unwrap();
            counter += 1;
        });
    });

    group.finish();
}

fn benchmark_scan_table(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_table");

    // Setup: Insert 1000 tuples
    let num_tuples = 1000;

    // Encrypted storage
    group.bench_function("encrypted", |b| {
        let storage = setup_encrypted_storage();
        let catalog = storage.catalog();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("data", DataType::Text),
        ]);

        catalog.create_table("scan_bench", schema)
            .expect("Failed to create table");

        // Insert test data
        for i in 0..num_tuples {
            let tuple = Tuple::new(vec![
                Value::Int4(i),
                Value::String(format!("data_value_{}", i)),
            ]);
            storage.insert_tuple("scan_bench", tuple)
                .expect("Failed to insert tuple");
        }

        b.iter(|| {
            let tuples = storage.scan_table(black_box("scan_bench")).unwrap();
            assert_eq!(tuples.len(), num_tuples as usize);
        });
    });

    // Unencrypted storage
    group.bench_function("unencrypted", |b| {
        let storage = setup_unencrypted_storage();
        let catalog = storage.catalog();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("data", DataType::Text),
        ]);

        catalog.create_table("scan_bench", schema)
            .expect("Failed to create table");

        // Insert test data
        for i in 0..num_tuples {
            let tuple = Tuple::new(vec![
                Value::Int4(i),
                Value::String(format!("data_value_{}", i)),
            ]);
            storage.insert_tuple("scan_bench", tuple)
                .expect("Failed to insert tuple");
        }

        b.iter(|| {
            let tuples = storage.scan_table(black_box("scan_bench")).unwrap();
            assert_eq!(tuples.len(), num_tuples as usize);
        });
    });

    group.finish();
}

fn benchmark_catalog_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("catalog");

    // Encrypted storage
    group.bench_function("create_table_encrypted", |b| {
        let storage = setup_encrypted_storage();
        let catalog = storage.catalog();
        let mut counter = 0;

        b.iter(|| {
            let schema = Schema::new(vec![
                Column::new("id", DataType::Int4),
                Column::new("name", DataType::Text),
            ]);

            let table_name = format!("table_{}", counter);
            catalog.create_table(&table_name, black_box(schema)).unwrap();
            counter += 1;
        });
    });

    // Unencrypted storage
    group.bench_function("create_table_unencrypted", |b| {
        let storage = setup_unencrypted_storage();
        let catalog = storage.catalog();
        let mut counter = 0;

        b.iter(|| {
            let schema = Schema::new(vec![
                Column::new("id", DataType::Int4),
                Column::new("name", DataType::Text),
            ]);

            let table_name = format!("table_{}", counter);
            catalog.create_table(&table_name, black_box(schema)).unwrap();
            counter += 1;
        });
    });

    group.finish();
}

fn benchmark_crypto_primitives(c: &mut Criterion) {
    let mut group = c.benchmark_group("crypto_primitives");

    let key: [u8; 32] = rand::random();

    // Test with different payload sizes
    for size in [64, 256, 1024, 4096, 16384].iter() {
        let plaintext = vec![0u8; *size];

        group.throughput(Throughput::Bytes(*size as u64));

        group.bench_with_input(
            BenchmarkId::new("encrypt", size),
            size,
            |b, _| {
                b.iter(|| {
                    heliosdb_nano::crypto::encrypt(&key, black_box(&plaintext)).unwrap()
                });
            },
        );

        // Pre-encrypt for decrypt benchmark
        let ciphertext = heliosdb_nano::crypto::encrypt(&key, &plaintext).unwrap();

        group.bench_with_input(
            BenchmarkId::new("decrypt", size),
            size,
            |b, _| {
                b.iter(|| {
                    heliosdb_nano::crypto::decrypt(&key, black_box(&ciphertext)).unwrap()
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_put_get,
    benchmark_insert_tuple,
    benchmark_scan_table,
    benchmark_catalog_operations,
    benchmark_crypto_primitives,
);

criterion_main!(benches);
