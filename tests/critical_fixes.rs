/// Integration tests for critical user-blocking fixes
use heliosdb_lite::EmbeddedDatabase;

#[test]
fn test_jsonb_type_support() {
    // ISSUE 1: JSONB type was not recognized in CREATE TABLE
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create in-memory DB");

    // This should no longer error with "Data type not yet supported: JSONB"
    db.execute("CREATE TABLE products (id INT, data JSONB)")
        .expect("CREATE TABLE with JSONB should succeed");

    // Insert JSON data as string
    db.execute(r#"INSERT INTO products (id, data) VALUES (1, '{"name": "Widget", "price": 9.99}')"#)
        .expect("INSERT with JSONB should succeed");

    // Query to verify data was inserted
    let results = db.query("SELECT * FROM products", &[])
        .expect("SELECT should succeed");

    assert_eq!(results.len(), 1, "Should have 1 row");
    println!("✓ JSONB type support works!");
}

#[test]
fn test_vector_string_autocast() {
    // ISSUE 2: Vector strings were not being auto-casted to Vector type
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create in-memory DB");

    // Create table with VECTOR column
    db.execute("CREATE TABLE embeddings (id INT, vec VECTOR(3))")
        .expect("CREATE TABLE should succeed");

    // Insert with string representation - should auto-cast to Vector type
    db.execute("INSERT INTO embeddings (id, vec) VALUES (1, '[0.1, 0.9, 0.0]')")
        .expect("INSERT with vector string should succeed");

    // This query would fail with "Vector distance operators require vector operands, got String"
    // if the auto-casting didn't work
    let results = db.query("SELECT * FROM embeddings WHERE vec <-> '[1.0, 0.0, 0.0]' < 5", &[])
        .expect("Vector distance query should succeed");

    assert_eq!(results.len(), 1, "Should find the inserted vector");
    println!("✓ Vector auto-casting works!");
}

#[test]
fn test_vector_with_explicit_cast() {
    // Additional test: explicit CAST should still work
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create in-memory DB");

    db.execute("CREATE TABLE vecs (id INT, v VECTOR(2))")
        .expect("CREATE TABLE should succeed");

    // Explicit CAST
    db.execute("INSERT INTO vecs VALUES (1, CAST('[1.0, 2.0]' AS VECTOR(2)))")
        .expect("INSERT with explicit CAST should succeed");

    let results = db.query("SELECT * FROM vecs", &[])
        .expect("SELECT should succeed");

    assert_eq!(results.len(), 1);
    println!("✓ Explicit CAST to VECTOR works!");
}

#[test]
fn test_auto_cast_type_mismatches() {
    // Test that auto-casting works for other type mismatches too
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create in-memory DB");

    db.execute("CREATE TABLE mixed (id INT, num INT, txt TEXT)")
        .expect("CREATE TABLE should succeed");

    // Insert with implicit casts - string "123" should cast to INT
    db.execute("INSERT INTO mixed VALUES (1, '42', 'hello')")
        .expect("INSERT with auto-cast should succeed");

    let results = db.query("SELECT * FROM mixed", &[])
        .expect("SELECT should succeed");

    assert_eq!(results.len(), 1);
    println!("✓ General auto-casting works!");
}
