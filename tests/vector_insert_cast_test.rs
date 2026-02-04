//! Test INSERT with CAST expressions and vector auto-detection

use heliosdb_lite::{EmbeddedDatabase, Result};

#[test]
fn test_insert_vector_with_cast() -> Result<()> {
    // Create in-memory database
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table with vector column
    db.execute("CREATE TABLE docs (id INT, content TEXT, embedding VECTOR(3))")?;

    // Test 1: INSERT with explicit CAST (user's original query)
    println!("Test 1: INSERT with CAST expression");
    let count = db.execute("INSERT INTO docs VALUES (1, 'AI', '[0.9, 0.1, 0.0]'::VECTOR(3))")?;
    assert_eq!(count, 1, "Should insert 1 row with CAST");

    // Test 2: INSERT with auto-detection (no explicit CAST needed)
    println!("Test 2: INSERT with auto-detection");
    let count2 = db.execute("INSERT INTO docs VALUES (2, 'ML', '[0.8, 0.2, 0.0]')")?;
    assert_eq!(count2, 1, "Should insert 1 row with auto-detection");

    // Verify both inserts worked
    println!("Verifying inserts...");
    let results = db.query("SELECT id, content FROM docs ORDER BY id", &[])?;
    assert_eq!(results.len(), 2, "Should have 2 rows");

    println!("✓ All tests passed!");
    Ok(())
}
