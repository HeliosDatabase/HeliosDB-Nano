//! Vector SQL Integration Tests
//!
//! Tests for vector similarity search via SQL layer

use heliosdb_lite::{EmbeddedDatabase, Result};

/// Test CREATE INDEX ... USING hnsw syntax
#[test]
fn test_create_vector_index_sql() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table with vector column
    db.execute("CREATE TABLE documents (
        id INT4 PRIMARY KEY,
        title TEXT,
        embedding VECTOR(3)
    )")?;

    // Create HNSW index
    let result = db.execute("CREATE INDEX embedding_idx ON documents USING hnsw (embedding)");
    assert!(result.is_ok(), "Failed to create HNSW index: {:?}", result.err());

    Ok(())
}

/// Test vector distance operators in SQL expressions
#[test]
fn test_vector_distance_operators() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE vectors (id INT4, vec VECTOR(3))")?;

    // Insert test vectors
    db.execute("INSERT INTO vectors (id, vec) VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO vectors (id, vec) VALUES (2, '[0.0, 1.0, 0.0]')")?;
    db.execute("INSERT INTO vectors (id, vec) VALUES (3, '[0.0, 0.0, 1.0]')")?;

    // Test L2 distance operator (<->)
    // Note: This requires SELECT with vector expressions, which may not be fully implemented yet
    // This test documents the intended behavior

    Ok(())
}

/// Test k-NN search pattern: ORDER BY distance + LIMIT
#[test]
fn test_knn_query_pattern() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE embeddings (
        id INT4,
        text TEXT,
        embedding VECTOR(3)
    )")?;

    // Create index
    db.execute("CREATE INDEX emb_idx ON embeddings USING hnsw (embedding)")?;

    // Insert test data
    db.execute("INSERT INTO embeddings VALUES (1, 'apple', '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO embeddings VALUES (2, 'banana', '[0.9, 0.1, 0.0]')")?;
    db.execute("INSERT INTO embeddings VALUES (3, 'cherry', '[0.0, 1.0, 0.0]')")?;
    db.execute("INSERT INTO embeddings VALUES (4, 'date', '[0.0, 0.0, 1.0]')")?;

    // Test k-NN query (this pattern should be optimized to use HNSW)
    // SELECT * FROM embeddings ORDER BY embedding <-> '[1.0, 0.0, 0.0]' LIMIT 2
    //
    // Expected results:
    // 1. apple (distance = 0)
    // 2. banana (distance ≈ 0.141)

    Ok(())
}

/// Test that vector indexes are used for efficient search
#[test]
fn test_vector_index_usage() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table with many vectors
    db.execute("CREATE TABLE large_vectors (
        id INT4,
        embedding VECTOR(128)
    )")?;

    // Create HNSW index
    db.execute("CREATE INDEX large_idx ON large_vectors USING hnsw (embedding)")?;

    // In a real test, we would:
    // 1. Insert many vectors (1000+)
    // 2. Run k-NN query with and without index
    // 3. Verify that indexed query is significantly faster
    // 4. Verify results are correct (approximate nearest neighbors)

    Ok(())
}

/// Test vector similarity with different distance metrics
#[test]
fn test_distance_metrics() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE metric_test (id INT4, vec VECTOR(2))")?;

    // Insert test vectors
    db.execute("INSERT INTO metric_test VALUES (1, '[1.0, 0.0]')")?;
    db.execute("INSERT INTO metric_test VALUES (2, '[0.0, 1.0]')")?;

    // Test different distance operators:
    // <-> : L2 (Euclidean) distance
    // <=> : Cosine distance
    // <#> : Inner product

    // These would be tested via SELECT queries once expression evaluation is complete

    Ok(())
}

/// Test CREATE INDEX with IF NOT EXISTS
#[test]
fn test_create_index_if_not_exists() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE test_table (id INT4, vec VECTOR(3))")?;

    // Create index
    db.execute("CREATE INDEX test_idx ON test_table USING hnsw (vec)")?;

    // Create again with IF NOT EXISTS - should succeed
    let result = db.execute("CREATE INDEX IF NOT EXISTS test_idx ON test_table USING hnsw (vec)");
    assert!(result.is_ok(), "IF NOT EXISTS should prevent error");

    // Create again without IF NOT EXISTS - should fail
    // (once proper index existence checking is implemented)

    Ok(())
}

/// Test error cases
#[test]
fn test_vector_index_errors() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Try to create index on non-existent table
    let result = db.execute("CREATE INDEX bad_idx ON nonexistent USING hnsw (vec)");
    assert!(result.is_err(), "Should fail on non-existent table");

    // Create table with non-vector column
    db.execute("CREATE TABLE no_vector (id INT4, name TEXT)")?;

    // Try to create HNSW index on non-vector column
    let result = db.execute("CREATE INDEX bad_idx ON no_vector USING hnsw (name)");
    assert!(result.is_err(), "Should fail on non-vector column");

    Ok(())
}

/// Test vector insertion and retrieval via SQL
#[test]
fn test_vector_crud() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE vectors (id INT4, embedding VECTOR(3))")?;

    // Insert vector via SQL
    db.execute("INSERT INTO vectors VALUES (1, '[1.0, 2.0, 3.0]')")?;

    // TODO: Test SELECT to retrieve vector
    // let results = db.query("SELECT id, embedding FROM vectors WHERE id = 1")?;
    // assert_eq!(results.len(), 1);

    Ok(())
}

/// Integration test: Full vector search workflow
#[test]
fn test_full_vector_search_workflow() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // 1. Create schema
    db.execute("CREATE TABLE products (
        id INT4 PRIMARY KEY,
        name TEXT,
        description TEXT,
        embedding VECTOR(8)
    )")?;

    // 2. Create index
    db.execute("CREATE INDEX product_emb_idx ON products USING hnsw (embedding)")?;

    // 3. Insert data
    db.execute("INSERT INTO products VALUES (1, 'Laptop', 'Gaming laptop', '[1.0,0.8,0.2,0.1,0.3,0.5,0.7,0.9]')")?;
    db.execute("INSERT INTO products VALUES (2, 'Mouse', 'Wireless mouse', '[1.0,0.7,0.3,0.2,0.4,0.6,0.8,0.1]')")?;
    db.execute("INSERT INTO products VALUES (3, 'Keyboard', 'Mechanical keyboard', '[0.9,0.6,0.4,0.3,0.5,0.7,0.2,0.8]')")?;
    db.execute("INSERT INTO products VALUES (4, 'Book', 'Programming book', '[0.1,0.2,0.3,0.9,0.8,0.7,0.6,0.5]')")?;

    // 4. Perform similarity search
    // Find products similar to laptops: [1.0,0.8,0.2,0.1,0.3,0.5,0.7,0.9]
    // Expected order: Laptop (0), Mouse (~close), Keyboard, Book (far)

    // This would use: SELECT * FROM products ORDER BY embedding <-> '[1.0,0.8,0.2,0.1,0.3,0.5,0.7,0.9]' LIMIT 3

    Ok(())
}
