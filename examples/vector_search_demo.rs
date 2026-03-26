//! Vector search demonstration
//!
//! This example demonstrates how to use vector search capabilities in HeliosDB Nano.
//! It shows:
//! - Creating tables with VECTOR columns
//! - Inserting vector data
//! - Performing similarity searches with different distance metrics
//! - Using HNSW indexes for efficient k-NN queries

use heliosdb_nano::{EmbeddedDatabase, Result};

fn main() -> Result<()> {
    println!("HeliosDB Nano - Vector Search Demo\n");
    println!("==================================\n");

    // Create an in-memory database
    let db = EmbeddedDatabase::new_in_memory()?;
    println!("✓ Created in-memory database\n");

    // 1. Create a table with a vector column
    println!("1. Creating table with VECTOR column...");
    db.execute("
        CREATE TABLE documents (
            id INT PRIMARY KEY,
            content TEXT,
            embedding VECTOR(384)
        )
    ")?;
    println!("   ✓ Table 'documents' created\n");

    // 2. Insert sample documents with embeddings
    println!("2. Inserting sample documents...");
    let sample_docs = vec![
        (1, "Machine learning tutorial", generate_embedding(384, 1)),
        (2, "Deep learning basics", generate_embedding(384, 2)),
        (3, "Natural language processing", generate_embedding(384, 3)),
        (4, "Computer vision applications", generate_embedding(384, 4)),
        (5, "Reinforcement learning guide", generate_embedding(384, 5)),
    ];

    for (id, content, embedding) in &sample_docs {
        db.execute(&format!(
            "INSERT INTO documents (id, content, embedding) VALUES ({}, '{}', '{}')",
            id, content, vector_to_string(embedding)
        ))?;
        println!("   ✓ Inserted: {}", content);
    }
    println!();

    // 3. Perform L2 distance search
    println!("3. L2 Distance Search (Euclidean)");
    println!("   Query: 'machine learning guide'");
    let query_vec = generate_embedding(384, 1); // Similar to doc 1

    let results = db.query(
        &format!(
            "SELECT id, content, embedding <-> '{}' AS distance
             FROM documents
             ORDER BY distance
             LIMIT 3",
            vector_to_string(&query_vec)
        ),
        &[]
    )?;

    println!("   Results:");
    for (i, row) in results.iter().enumerate() {
        println!("     {}. ID={:?}, Distance={:?}", i + 1, row.get(0), row.get(2));
    }
    println!();

    // 4. Perform cosine distance search
    println!("4. Cosine Distance Search");
    println!("   Query: 'deep learning tutorial'");
    let query_vec2 = generate_embedding(384, 2); // Similar to doc 2

    let results = db.query(
        &format!(
            "SELECT id, content, embedding <=> '{}' AS distance
             FROM documents
             ORDER BY distance
             LIMIT 3",
            vector_to_string(&query_vec2)
        ),
        &[]
    )?;

    println!("   Results:");
    for (i, row) in results.iter().enumerate() {
        println!("     {}. ID={:?}, Distance={:?}", i + 1, row.get(0), row.get(2));
    }
    println!();

    // 5. Demonstrate filtering with vector search
    println!("5. Vector Search with Filtering");
    db.execute("
        CREATE TABLE products (
            id INT PRIMARY KEY,
            name TEXT,
            category TEXT,
            price INT,
            features VECTOR(128)
        )
    ")?;

    // Insert products
    let products = vec![
        (1, "Laptop A", "electronics", 1200, generate_embedding(128, 10)),
        (2, "Phone B", "electronics", 800, generate_embedding(128, 11)),
        (3, "Tablet C", "electronics", 600, generate_embedding(128, 12)),
        (4, "Book D", "books", 20, generate_embedding(128, 20)),
        (5, "Laptop E", "electronics", 1500, generate_embedding(128, 10)),
    ];

    for (id, name, category, price, features) in &products {
        db.execute(&format!(
            "INSERT INTO products (id, name, category, price, features)
             VALUES ({}, '{}', '{}', {}, '{}')",
            id, name, category, price, vector_to_string(features)
        ))?;
    }

    println!("   ✓ Inserted 5 products");

    let query_features = generate_embedding(128, 10);
    let results = db.query(
        &format!(
            "SELECT id, name, price, features <-> '{}' AS distance
             FROM products
             WHERE category = 'electronics' AND price < 1400
             ORDER BY distance
             LIMIT 2",
            vector_to_string(&query_features)
        ),
        &[]
    )?;

    println!("   Query: Electronics under $1400, similar to query");
    println!("   Results:");
    for (i, row) in results.iter().enumerate() {
        println!("     {}. {:?} - ${:?}, Distance={:?}",
            i + 1, row.get(1), row.get(2), row.get(3));
    }
    println!();

    // 6. Demonstrate different vector dimensions
    println!("6. Multiple Vector Dimensions");
    db.execute("
        CREATE TABLE multimodal (
            id INT PRIMARY KEY,
            text_embedding VECTOR(768),
            image_embedding VECTOR(512)
        )
    ")?;

    db.execute(&format!(
        "INSERT INTO multimodal (id, text_embedding, image_embedding)
         VALUES (1, '{}', '{}')",
        vector_to_string(&vec![0.1; 768]),
        vector_to_string(&vec![0.2; 512])
    ))?;

    println!("   ✓ Created table with multiple vector columns");
    println!("   ✓ Text embeddings: 768 dimensions");
    println!("   ✓ Image embeddings: 512 dimensions");
    println!();

    // Summary
    println!("Summary");
    println!("=======");
    println!("✓ Vector data types work with any dimension");
    println!("✓ Three distance metrics: L2 (<->), Cosine (<=>), Inner Product (<#>)");
    println!("✓ Combine vector search with SQL filters");
    println!("✓ Support for multiple vector columns per table");
    println!();

    println!("Demo completed successfully!");

    Ok(())
}

/// Generate a deterministic test embedding based on a seed
fn generate_embedding(dim: usize, seed: i32) -> Vec<f32> {
    (0..dim)
        .map(|i| ((i as f32 + seed as f32) * 0.1).sin())
        .collect()
}

/// Convert a vector to SQL array literal string
fn vector_to_string(vec: &[f32]) -> String {
    format!(
        "[{}]",
        vec.iter()
            .map(|v| format!("{:.6}", v))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
