//! Product Quantization Integration Tests
//!
//! Tests the full PQ integration including:
//! - SQL CREATE INDEX with quantization options
//! - Vector insertion with PQ compression
//! - Vector search with quantized indexes
//! - Persistence and recovery

use heliosdb_nano::{HeliosDB, Result};

#[test]
fn test_create_pq_index_sql() -> Result<()> {
    let db = HeliosDB::new_in_memory()?;

    // Create table with vector column
    db.execute("CREATE TABLE documents (id INTEGER PRIMARY KEY, embedding VECTOR(128))", &[])?;

    // Create PQ index with explicit options
    db.execute(
        "CREATE INDEX doc_idx ON documents(embedding) USING hnsw WITH (quantization='product', pq_subquantizers=8, pq_centroids=256)",
        &[]
    )?;

    // Verify index was created
    let result = db.query("SELECT * FROM helios_vector_indexes WHERE index_name = 'doc_idx'", &[])?;
    assert_eq!(result.len(), 1);

    let quantization = result[0].get::<String>("quantization").unwrap();
    assert_eq!(quantization, "Product");

    Ok(())
}

#[test]
fn test_create_pq_index_default_options() -> Result<()> {
    let db = HeliosDB::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE embeddings (id INTEGER PRIMARY KEY, vec VECTOR(768))", &[])?;

    // Create PQ index with default options (should auto-configure)
    db.execute(
        "CREATE INDEX emb_idx ON embeddings(vec) USING hnsw WITH (quantization='product')",
        &[]
    )?;

    // Verify index was created
    let result = db.query("SELECT * FROM helios_vector_indexes WHERE index_name = 'emb_idx'", &[])?;
    assert_eq!(result.len(), 1);

    Ok(())
}

#[test]
fn test_insert_and_search_pq() -> Result<()> {
    let db = HeliosDB::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE vectors (id INTEGER PRIMARY KEY, v VECTOR(128))", &[])?;

    // Create PQ index
    db.execute(
        "CREATE INDEX v_idx ON vectors(v) USING hnsw WITH (quantization='product', pq_subquantizers=8)",
        &[]
    )?;

    // Insert vectors
    for i in 1..=100 {
        let vector: Vec<f32> = (0..128)
            .map(|j| ((i * 100 + j) as f32).sin())
            .collect();

        db.execute(
            "INSERT INTO vectors (id, v) VALUES (?, VECTOR(?))",
            &[&i, &vector]
        )?;
    }

    // Search for nearest neighbors
    let query: Vec<f32> = (0..128).map(|i| (i as f32).sin()).collect();
    let results = db.query(
        "SELECT id FROM vectors ORDER BY vector_distance(v, VECTOR(?)) LIMIT 5",
        &[&query]
    )?;

    assert_eq!(results.len(), 5);
    // First result should be id=1 (most similar)
    let first_id = results[0].get::<i64>("id").unwrap();
    assert_eq!(first_id, 1);

    Ok(())
}

#[test]
fn test_pq_memory_efficiency() -> Result<()> {
    let db = HeliosDB::new_in_memory()?;

    // Create two tables with same data
    db.execute("CREATE TABLE vectors_standard (id INTEGER PRIMARY KEY, v VECTOR(768))", &[])?;
    db.execute("CREATE TABLE vectors_pq (id INTEGER PRIMARY KEY, v VECTOR(768))", &[])?;

    // Create standard index
    db.execute(
        "CREATE INDEX std_idx ON vectors_standard(v) USING hnsw",
        &[]
    )?;

    // Create PQ index
    db.execute(
        "CREATE INDEX pq_idx ON vectors_pq(v) USING hnsw WITH (quantization='product', pq_subquantizers=8)",
        &[]
    )?;

    // Insert same data into both
    for i in 1..=100 {
        let vector: Vec<f32> = (0..768)
            .map(|j| ((i * 100 + j) as f32).sin())
            .collect();

        db.execute(
            "INSERT INTO vectors_standard (id, v) VALUES (?, VECTOR(?))",
            &[&i, &vector]
        )?;

        db.execute(
            "INSERT INTO vectors_pq (id, v) VALUES (?, VECTOR(?))",
            &[&i, &vector]
        )?;
    }

    // Check memory usage
    let std_stats = db.query("SELECT * FROM helios_vector_indexes WHERE index_name = 'std_idx'", &[])?;
    let pq_stats = db.query("SELECT * FROM helios_vector_indexes WHERE index_name = 'pq_idx'", &[])?;

    let std_memory = std_stats[0].get::<i64>("memory_bytes").unwrap();
    let pq_memory = pq_stats[0].get::<i64>("memory_bytes").unwrap();

    // PQ should use significantly less memory (at least 4x compression)
    assert!(pq_memory < std_memory / 4);

    Ok(())
}

#[test]
fn test_pq_index_persistence() -> Result<()> {
    use std::path::PathBuf;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.db");

    // Create database and index
    {
        let db = HeliosDB::open(&db_path)?;

        db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, emb VECTOR(128))", &[])?;

        db.execute(
            "CREATE INDEX doc_idx ON docs(emb) USING hnsw WITH (quantization='product')",
            &[]
        )?;

        // Insert test vectors
        for i in 1..=50 {
            let vector: Vec<f32> = (0..128)
                .map(|j| ((i * 100 + j) as f32).sin())
                .collect();

            db.execute(
                "INSERT INTO docs (id, emb) VALUES (?, VECTOR(?))",
                &[&i, &vector]
            )?;
        }
    }

    // Reopen database and verify index persisted
    {
        let db = HeliosDB::open(&db_path)?;

        // Check index exists
        let result = db.query("SELECT * FROM helios_vector_indexes WHERE index_name = 'doc_idx'", &[])?;
        assert_eq!(result.len(), 1);

        let quantization = result[0].get::<String>("quantization").unwrap();
        assert_eq!(quantization, "Product");

        let num_vectors = result[0].get::<i64>("num_vectors").unwrap();
        assert_eq!(num_vectors, 50);

        // Verify search still works
        let query: Vec<f32> = (0..128).map(|i| (i as f32).sin()).collect();
        let results = db.query(
            "SELECT id FROM docs ORDER BY vector_distance(emb, VECTOR(?)) LIMIT 3",
            &[&query]
        )?;

        assert_eq!(results.len(), 3);
    }

    Ok(())
}

#[test]
fn test_pq_search_accuracy() -> Result<()> {
    let db = HeliosDB::new_in_memory()?;

    // Create table with both standard and PQ index
    db.execute("CREATE TABLE vectors (id INTEGER PRIMARY KEY, v VECTOR(128))", &[])?;

    db.execute("CREATE INDEX std_idx ON vectors(v) USING hnsw", &[])?;
    db.execute(
        "CREATE INDEX pq_idx ON vectors(v) USING hnsw WITH (quantization='product')",
        &[]
    )?;

    // Insert test vectors
    for i in 1..=100 {
        let vector: Vec<f32> = (0..128)
            .map(|j| ((i * 100 + j) as f32).sin())
            .collect();

        db.execute(
            "INSERT INTO vectors (id, v) VALUES (?, VECTOR(?))",
            &[&i, &vector]
        )?;
    }

    // Search with query
    let query: Vec<f32> = (0..128).map(|i| (i as f32).sin()).collect();

    // Get results from standard index
    let std_results = db.query(
        "SELECT id FROM vectors USE INDEX (std_idx) ORDER BY vector_distance(v, VECTOR(?)) LIMIT 10",
        &[&query]
    )?;

    // Get results from PQ index
    let pq_results = db.query(
        "SELECT id FROM vectors USE INDEX (pq_idx) ORDER BY vector_distance(v, VECTOR(?)) LIMIT 10",
        &[&query]
    )?;

    // PQ should have high recall (at least 80% overlap in top-10)
    let std_ids: Vec<i64> = std_results.iter()
        .map(|r| r.get::<i64>("id").unwrap())
        .collect();
    let pq_ids: Vec<i64> = pq_results.iter()
        .map(|r| r.get::<i64>("id").unwrap())
        .collect();

    let overlap = pq_ids.iter().filter(|id| std_ids.contains(id)).count();
    let recall = overlap as f64 / std_ids.len() as f64;

    assert!(recall >= 0.8, "PQ recall should be at least 80%, got {:.2}", recall);

    Ok(())
}

#[test]
fn test_pq_invalid_config() -> Result<()> {
    let db = HeliosDB::new_in_memory()?;

    db.execute("CREATE TABLE vectors (id INTEGER PRIMARY KEY, v VECTOR(100))", &[])?;

    // Try to create PQ index with invalid subquantizer count (dimension not divisible)
    let result = db.execute(
        "CREATE INDEX bad_idx ON vectors(v) USING hnsw WITH (quantization='product', pq_subquantizers=7)",
        &[]
    );

    // Should fail with validation error
    assert!(result.is_err());

    Ok(())
}

#[test]
fn test_pq_different_dimensions() -> Result<()> {
    let db = HeliosDB::new_in_memory()?;

    // Test different common dimensions
    for dim in &[128, 256, 384, 512, 768, 1024] {
        let table_name = format!("vectors_{}", dim);
        let index_name = format!("idx_{}", dim);

        db.execute(
            &format!("CREATE TABLE {} (id INTEGER PRIMARY KEY, v VECTOR({}))", table_name, dim),
            &[]
        )?;

        db.execute(
            &format!(
                "CREATE INDEX {} ON {}(v) USING hnsw WITH (quantization='product')",
                index_name, table_name
            ),
            &[]
        )?;

        // Verify index created successfully
        let result = db.query(
            &format!("SELECT * FROM helios_vector_indexes WHERE index_name = '{}'", index_name),
            &[]
        )?;

        assert_eq!(result.len(), 1);
        let dimensions = result[0].get::<i32>("dimensions").unwrap();
        assert_eq!(dimensions, *dim as i32);
    }

    Ok(())
}
