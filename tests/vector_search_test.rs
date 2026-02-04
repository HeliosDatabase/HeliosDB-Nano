//! Vector search integration tests
//!
//! Tests for VECTOR data type, similarity operators, and HNSW indexes.

use heliosdb_lite::{EmbeddedDatabase, Result};

#[test]
fn test_create_table_with_vector() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table with vector column
    db.execute("CREATE TABLE documents (
        id INT PRIMARY KEY,
        content TEXT,
        embedding VECTOR(3)
    )")?;

    Ok(())
}

#[test]
fn test_insert_vector_data() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE items (
        id INT PRIMARY KEY,
        name TEXT,
        embedding VECTOR(3)
    )")?;

    // Insert with vector literal
    db.execute("INSERT INTO items (id, name, embedding)
        VALUES (1, 'item1', '[1.0, 0.0, 0.0]')")?;

    db.execute("INSERT INTO items (id, name, embedding)
        VALUES (2, 'item2', '[0.0, 1.0, 0.0]')")?;

    db.execute("INSERT INTO items (id, name, embedding)
        VALUES (3, 'item3', '[0.0, 0.0, 1.0]')")?;

    // Verify data was inserted
    let results = db.query("SELECT * FROM items", &[])?;
    assert_eq!(results.len(), 3);

    Ok(())
}

#[test]
fn test_vector_l2_distance() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE vectors (
        id INT PRIMARY KEY,
        vec VECTOR(2)
    )")?;

    // Insert vectors
    db.execute("INSERT INTO vectors VALUES (1, '[1.0, 0.0]')")?;
    db.execute("INSERT INTO vectors VALUES (2, '[0.0, 1.0]')")?;

    // Query with L2 distance operator
    let results = db.query(
        "SELECT id, vec <-> '[1.0, 0.0]' AS distance FROM vectors ORDER BY distance",
        &[]
    )?;

    assert_eq!(results.len(), 2);
    // First result should be id=1 with distance close to 0

    Ok(())
}

#[test]
fn test_vector_cosine_distance() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE vectors (id INT PRIMARY KEY, vec VECTOR(2))")?;
    db.execute("INSERT INTO vectors VALUES (1, '[1.0, 0.0]')")?;
    db.execute("INSERT INTO vectors VALUES (2, '[0.707, 0.707]')")?;

    // Cosine distance operator
    let results = db.query(
        "SELECT id, vec <=> '[1.0, 0.0]' AS distance FROM vectors ORDER BY distance",
        &[]
    )?;

    assert_eq!(results.len(), 2);
    Ok(())
}

#[test]
fn test_vector_inner_product() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE vectors (id INT PRIMARY KEY, vec VECTOR(3))")?;
    db.execute("INSERT INTO vectors VALUES (1, '[1.0, 2.0, 3.0]')")?;
    db.execute("INSERT INTO vectors VALUES (2, '[4.0, 5.0, 6.0]')")?;

    // Inner product operator
    let results = db.query(
        "SELECT id, vec <#> '[1.0, 0.0, 0.0]' AS score FROM vectors ORDER BY score",
        &[]
    )?;

    assert_eq!(results.len(), 2);
    Ok(())
}

#[test]
fn test_knn_search() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table with embeddings
    db.execute("CREATE TABLE documents (
        id INT PRIMARY KEY,
        content TEXT,
        embedding VECTOR(384)
    )")?;

    // Insert some documents (using simple embeddings for testing)
    for i in 1..=100 {
        let embedding = generate_test_vector(384, i);
        db.execute(&format!(
            "INSERT INTO documents (id, content, embedding) VALUES ({}, 'doc{}', '{}')",
            i, i, vector_to_string(&embedding)
        ))?;
    }

    // Perform k-NN search (find 10 nearest neighbors)
    let query_vec = generate_test_vector(384, 50);
    let results = db.query(
        &format!(
            "SELECT id, content, embedding <-> '{}' AS distance
             FROM documents
             ORDER BY distance
             LIMIT 10",
            vector_to_string(&query_vec)
        ),
        &[]
    )?;

    assert_eq!(results.len(), 10);
    Ok(())
}

#[test]
fn test_vector_dimension_validation() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE vectors (id INT PRIMARY KEY, vec VECTOR(3))")?;

    // Try to insert vector with wrong dimension - should fail
    let result = db.execute("INSERT INTO vectors VALUES (1, '[1.0, 2.0]')");
    assert!(result.is_err(), "Should fail with dimension mismatch");

    Ok(())
}

#[test]
fn test_multiple_vector_columns() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE items (
        id INT PRIMARY KEY,
        text_embedding VECTOR(768),
        image_embedding VECTOR(512)
    )")?;

    // Insert with multiple vectors
    db.execute(&format!(
        "INSERT INTO items VALUES (1, '{}', '{}')",
        vector_to_string(&vec![0.5; 768]),
        vector_to_string(&vec![0.3; 512])
    ))?;

    let results = db.query("SELECT * FROM items", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
#[ignore = "TODO: Vector NULL value handling needs implementation"]
fn test_vector_with_null() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE vectors (
        id INT PRIMARY KEY,
        vec VECTOR(3)
    )")?;

    // Insert with NULL vector (if column is nullable)
    db.execute("INSERT INTO vectors (id) VALUES (1)")?;

    let results = db.query("SELECT * FROM vectors WHERE vec IS NULL", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

// Helper functions

fn generate_test_vector(dim: usize, seed: i32) -> Vec<f32> {
    (0..dim)
        .map(|i| ((i as f32 + seed as f32) * 0.1).sin())
        .collect()
}

fn vector_to_string(vec: &[f32]) -> String {
    format!(
        "[{}]",
        vec.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[test]
fn test_vector_search_with_where_clause() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE products (
        id INT PRIMARY KEY,
        category TEXT,
        embedding VECTOR(4)
    )")?;

    db.execute("INSERT INTO products VALUES (1, 'A', '[1.0, 0.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO products VALUES (2, 'B', '[0.0, 1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO products VALUES (3, 'A', '[0.0, 0.0, 1.0, 0.0]')")?;
    db.execute("INSERT INTO products VALUES (4, 'A', '[0.0, 0.0, 0.0, 1.0]')")?;

    // Search only within category 'A'
    let results = db.query(
        "SELECT id, embedding <-> '[1.0, 0.0, 0.0, 0.0]' AS distance
         FROM products
         WHERE category = 'A'
         ORDER BY distance
         LIMIT 2",
        &[]
    )?;

    assert_eq!(results.len(), 2);
    Ok(())
}

#[test]
fn test_pgvector_compatibility() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Test pgvector-compatible syntax
    db.execute("CREATE TABLE items (
        id INT PRIMARY KEY,
        embedding VECTOR(1536)
    )")?;

    // Insert using array literal (pgvector style)
    db.execute(&format!(
        "INSERT INTO items VALUES (1, '{}')",
        vector_to_string(&vec![0.1; 1536])
    ))?;

    // Query using L2 distance (pgvector <-> operator)
    let results = db.query(
        &format!(
            "SELECT id, embedding <-> '{}' AS distance FROM items LIMIT 1",
            vector_to_string(&vec![0.1; 1536])
        ),
        &[]
    )?;

    assert_eq!(results.len(), 1);
    Ok(())
}
