//! Vector SQL Integration Tests
//!
//! Tests for vector similarity search via SQL layer

use heliosdb_nano::{
    sql::SystemViewRegistry, storage::VectorIndexType, vector::DistanceMetric, EmbeddedDatabase, Result, Value,
};
use tempfile::TempDir;

/// Test CREATE INDEX ... USING hnsw syntax
#[test]
fn test_create_vector_index_sql() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table with vector column
    db.execute(
        "CREATE TABLE documents (
        id INT4 PRIMARY KEY,
        title TEXT,
        embedding VECTOR(3)
    )",
    )?;

    // Create HNSW index
    let result = db.execute("CREATE INDEX embedding_idx ON documents USING hnsw (embedding)");
    assert!(result.is_ok(), "Failed to create HNSW index: {:?}", result.err());

    Ok(())
}

#[test]
fn test_create_hnsw_index_on_populated_table_backfills_existing_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE hnsw_populated (id INT4 PRIMARY KEY, embedding VECTOR(3))")?;
    db.execute("INSERT INTO hnsw_populated VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO hnsw_populated VALUES (2, '[0.9, 0.1, 0.0]')")?;
    db.execute("INSERT INTO hnsw_populated VALUES (3, '[0.0, 1.0, 0.0]')")?;

    db.execute(
        "CREATE INDEX hnsw_populated_embedding_idx \
         ON hnsw_populated USING hnsw (embedding vector_cosine_ops)",
    )?;

    let stats = db
        .storage
        .vector_indexes()
        .get_index_stats("hnsw_populated_embedding_idx")?;
    assert_eq!(stats.num_vectors, 3, "CREATE INDEX must backfill existing rows");

    let metadata = db
        .storage
        .vector_indexes()
        .get_metadata("hnsw_populated_embedding_idx")?;
    match metadata.index_type {
        VectorIndexType::Standard(config) => assert_eq!(config.distance_metric, DistanceMetric::Cosine),
        other => panic!("expected standard HNSW metadata, got {other:?}"),
    }

    let hits = db
        .storage
        .vector_indexes()
        .search("hnsw_populated_embedding_idx", &vec![1.0, 0.0, 0.0], 2)?;
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].0, 1,
        "nearest row inserted before CREATE INDEX should be searchable"
    );

    let pg_indexes = SystemViewRegistry::new().execute("pg_indexes", &db.storage)?;
    assert!(
        pg_indexes.iter().any(|tuple| {
            matches!(
                tuple.values.get(2),
                Some(Value::String(name)) if name == "hnsw_populated_embedding_idx"
            )
        }),
        "pg_indexes should expose the created HNSW index"
    );

    Ok(())
}

#[test]
fn test_hnsw_index_definition_survives_reopen() -> Result<()> {
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path())?;
        db.execute("CREATE TABLE hnsw_reopen (id INT4 PRIMARY KEY, embedding VECTOR(3))")?;
        db.execute("INSERT INTO hnsw_reopen VALUES (1, '[1.0, 0.0, 0.0]')")?;
        db.execute("INSERT INTO hnsw_reopen VALUES (2, '[0.0, 1.0, 0.0]')")?;
        db.execute(
            "CREATE INDEX hnsw_reopen_embedding_idx \
             ON hnsw_reopen USING hnsw (embedding vector_cosine_ops)",
        )?;
        let stats = db
            .storage
            .vector_indexes()
            .get_index_stats("hnsw_reopen_embedding_idx")?;
        assert_eq!(stats.num_vectors, 2);
    }

    let db = EmbeddedDatabase::new(temp.path())?;
    let stats = db
        .storage
        .vector_indexes()
        .get_index_stats("hnsw_reopen_embedding_idx")?;
    assert_eq!(
        stats.num_vectors, 2,
        "HNSW index must be rebuilt from persisted metadata on reopen"
    );

    let metadata = db.storage.vector_indexes().get_metadata("hnsw_reopen_embedding_idx")?;
    match metadata.index_type {
        VectorIndexType::Standard(config) => assert_eq!(config.distance_metric, DistanceMetric::Cosine),
        other => panic!("expected standard HNSW metadata after reopen, got {other:?}"),
    }

    let pg_indexes = SystemViewRegistry::new().execute("pg_indexes", &db.storage)?;
    assert!(
        pg_indexes.iter().any(|tuple| {
            matches!(
                tuple.values.get(2),
                Some(Value::String(name)) if name == "hnsw_reopen_embedding_idx"
            )
        }),
        "pg_indexes should expose rebuilt HNSW index after reopen"
    );

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
    db.execute(
        "CREATE TABLE embeddings (
        id INT4,
        text TEXT,
        embedding VECTOR(3)
    )",
    )?;

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
    db.execute(
        "CREATE TABLE large_vectors (
        id INT4,
        embedding VECTOR(128)
    )",
    )?;

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
    db.execute(
        "CREATE TABLE products (
        id INT4 PRIMARY KEY,
        name TEXT,
        description TEXT,
        embedding VECTOR(8)
    )",
    )?;

    // 2. Create index
    db.execute("CREATE INDEX product_emb_idx ON products USING hnsw (embedding)")?;

    // 3. Insert data
    db.execute("INSERT INTO products VALUES (1, 'Laptop', 'Gaming laptop', '[1.0,0.8,0.2,0.1,0.3,0.5,0.7,0.9]')")?;
    db.execute("INSERT INTO products VALUES (2, 'Mouse', 'Wireless mouse', '[1.0,0.7,0.3,0.2,0.4,0.6,0.8,0.1]')")?;
    db.execute(
        "INSERT INTO products VALUES (3, 'Keyboard', 'Mechanical keyboard', '[0.9,0.6,0.4,0.3,0.5,0.7,0.2,0.8]')",
    )?;
    db.execute("INSERT INTO products VALUES (4, 'Book', 'Programming book', '[0.1,0.2,0.3,0.9,0.8,0.7,0.6,0.5]')")?;

    // 4. Perform similarity search
    // Find products similar to laptops: [1.0,0.8,0.2,0.1,0.3,0.5,0.7,0.9]
    // Expected order: Laptop (0), Mouse (~close), Keyboard, Book (far)

    // This would use: SELECT * FROM products ORDER BY embedding <-> '[1.0,0.8,0.2,0.1,0.3,0.5,0.7,0.9]' LIMIT 3

    Ok(())
}

// ---------------------------------------------------------------------------
// Regression tests for the HNSW kNN planner fast path (FIX 1) and the
// parallel CREATE INDEX backfill (FIX 2).
// ---------------------------------------------------------------------------

fn vec_lit(v: &[f32]) -> String {
    let body: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", body.join(","))
}

/// Deterministic pseudo-random 3-D unit-ish vector for a given id.
fn synth_vec3(id: i64) -> [f32; 3] {
    let a = ((id.wrapping_mul(2654435761)) as u32 as f32) / u32::MAX as f32;
    let b = ((id.wrapping_mul(40503).wrapping_add(7)) as u32 as f32) / u32::MAX as f32;
    let c = ((id.wrapping_mul(2246822519).wrapping_add(13)) as u32 as f32) / u32::MAX as f32;
    [a + 0.001, b + 0.001, c + 0.001]
}

/// FIX 1: `ORDER BY col <=> '[...]' LIMIT k` with an HNSW cosine index must
/// return the true nearest neighbour first (correctly ordered). Uses a
/// literal query vector.
#[test]
fn test_knn_planner_uses_hnsw_cosine_literal() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE knn_c (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO knn_c VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO knn_c VALUES (2, '[0.9, 0.1, 0.0]')")?;
    db.execute("INSERT INTO knn_c VALUES (3, '[0.0, 1.0, 0.0]')")?;
    db.execute("INSERT INTO knn_c VALUES (4, '[0.0, 0.0, 1.0]')")?;
    db.execute("CREATE INDEX knn_c_idx ON knn_c USING hnsw (embedding vector_cosine_ops)")?;

    let rows = db.query(
        "SELECT id FROM knn_c ORDER BY embedding <=> '[1.0, 0.0, 0.0]' LIMIT 2",
        &[],
    )?;
    assert_eq!(rows.len(), 2, "kNN LIMIT 2 must return 2 rows");
    assert_eq!(
        rows[0].values.first(),
        Some(&Value::Int4(1)),
        "row (1) is the exact match and must rank first"
    );
    assert_eq!(
        rows[1].values.first(),
        Some(&Value::Int4(2)),
        "row (2) is the 2nd nearest and must rank second"
    );
    Ok(())
}

/// FIX 1: same path but the query vector arrives as a bound `$1` parameter,
/// which is the shape live traffic actually uses.
#[test]
fn test_knn_planner_uses_hnsw_param_vector() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE knn_p (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO knn_p VALUES (10, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO knn_p VALUES (20, '[0.8, 0.2, 0.0]')")?;
    db.execute("INSERT INTO knn_p VALUES (30, '[0.0, 0.0, 1.0]')")?;
    db.execute("CREATE INDEX knn_p_idx ON knn_p USING hnsw (embedding vector_cosine_ops)")?;

    let rows = db.query_params(
        "SELECT id FROM knn_p ORDER BY embedding <=> $1 LIMIT 1",
        &[Value::Vector(vec![1.0, 0.0, 0.0])],
    )?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.first(), Some(&Value::Int4(10)));
    Ok(())
}

/// FIX 1: the L2 operator `<->` must select an L2 index and order ascending.
#[test]
fn test_knn_planner_uses_hnsw_l2() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE knn_l2 (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO knn_l2 VALUES (1, '[0.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO knn_l2 VALUES (2, '[5.0, 5.0, 5.0]')")?;
    db.execute("INSERT INTO knn_l2 VALUES (3, '[0.1, 0.0, 0.0]')")?;
    db.execute("CREATE INDEX knn_l2_idx ON knn_l2 USING hnsw (embedding vector_l2_ops)")?;

    let rows = db.query(
        "SELECT id FROM knn_l2 ORDER BY embedding <-> '[0.0,0.0,0.0]' LIMIT 2",
        &[],
    )?;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].values.first(),
        Some(&Value::Int4(1)),
        "exact origin match first"
    );
    assert_eq!(
        rows[1].values.first(),
        Some(&Value::Int4(3)),
        "nearest non-exact second"
    );
    Ok(())
}

/// FIX 1: OFFSET must be honoured — `LIMIT 1 OFFSET 1` skips the nearest and
/// returns the 2nd nearest.
#[test]
fn test_knn_planner_honors_offset() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE knn_off (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO knn_off VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO knn_off VALUES (2, '[0.9, 0.1, 0.0]')")?;
    db.execute("INSERT INTO knn_off VALUES (3, '[0.0, 1.0, 0.0]')")?;
    db.execute("CREATE INDEX knn_off_idx ON knn_off USING hnsw (embedding vector_cosine_ops)")?;

    let rows = db.query(
        "SELECT id FROM knn_off ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1 OFFSET 1",
        &[],
    )?;
    assert_eq!(rows.len(), 1, "LIMIT 1 OFFSET 1 returns exactly one row");
    assert_eq!(
        rows[0].values.first(),
        Some(&Value::Int4(2)),
        "OFFSET 1 must skip the nearest (1) and return the 2nd nearest (2)"
    );
    Ok(())
}

/// FIX 1: with NO HNSW index present, the same kNN query must still work
/// (brute-force fallback) and return correctly-ordered results — no regression.
#[test]
fn test_knn_query_without_index_still_correct() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE knn_noidx (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO knn_noidx VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO knn_noidx VALUES (2, '[0.9, 0.1, 0.0]')")?;
    db.execute("INSERT INTO knn_noidx VALUES (3, '[0.0, 1.0, 0.0]')")?;

    let rows = db.query(
        "SELECT id FROM knn_noidx ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 2",
        &[],
    )?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values.first(), Some(&Value::Int4(1)));
    assert_eq!(rows[1].values.first(), Some(&Value::Int4(2)));
    Ok(())
}

/// FIX 1 (correctness at scale): with a few thousand rows and a cosine HNSW
/// index, the indexed kNN result's top hit must agree with an exact
/// brute-force computation over all rows. Proves the index path is wired up
/// and returns the genuine nearest neighbour rather than arbitrary rows.
#[test]
fn test_knn_planner_matches_bruteforce_top1() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE knn_big (id INT4, embedding VECTOR(3))")?;
    let n: i64 = 3000;
    // Multi-row insert batches.
    let mut batch = Vec::new();
    let mut all: Vec<(i64, [f32; 3])> = Vec::new();
    for id in 1..=n {
        let v = synth_vec3(id);
        all.push((id, v));
        batch.push(format!("({}, '{}')", id, vec_lit(&v)));
        if batch.len() == 500 {
            db.execute(&format!("INSERT INTO knn_big VALUES {}", batch.join(",")))?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        db.execute(&format!("INSERT INTO knn_big VALUES {}", batch.join(",")))?;
    }
    db.execute("CREATE INDEX knn_big_idx ON knn_big USING hnsw (embedding vector_cosine_ops)")?;

    // Query vector = a perturbation of row 1234's embedding so its true
    // nearest neighbour is deterministic.
    let target = all[1233].1;
    let query = [target[0] + 0.0005, target[1] - 0.0005, target[2] + 0.0005];

    // Exact brute-force nearest by cosine distance.
    let cos = |a: &[f32; 3], b: &[f32; 3]| {
        let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let na = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        let nb = (b[0] * b[0] + b[1] * b[1] + b[2] * b[2]).sqrt();
        1.0 - dot / (na * nb)
    };
    let mut best_id = all[0].0;
    let mut best_d = f32::INFINITY;
    for (id, v) in &all {
        let d = cos(v, &query);
        if d < best_d {
            best_d = d;
            best_id = *id;
        }
    }

    let rows = db.query_params(
        "SELECT id FROM knn_big ORDER BY embedding <=> $1 LIMIT 1",
        &[Value::Vector(query.to_vec())],
    )?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].values.first(),
        Some(&Value::Int4(best_id as i32)),
        "indexed kNN top-1 must equal brute-force top-1 (id {})",
        best_id
    );
    Ok(())
}

/// FIX 2: the parallel batch backfill must index every existing row and the
/// resulting index must be searchable with correct ordering — identical
/// semantics to the old per-row sequential build.
#[test]
fn test_parallel_backfill_indexes_all_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE pbf (id INT4, embedding VECTOR(3))")?;

    let n: i64 = 4000;
    let mut batch = Vec::new();
    for id in 1..=n {
        let v = synth_vec3(id);
        batch.push(format!("({}, '{}')", id, vec_lit(&v)));
        if batch.len() == 500 {
            db.execute(&format!("INSERT INTO pbf VALUES {}", batch.join(",")))?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        db.execute(&format!("INSERT INTO pbf VALUES {}", batch.join(",")))?;
    }

    // CREATE INDEX on the populated table now goes through the parallel
    // backfill path.
    db.execute("CREATE INDEX pbf_idx ON pbf USING hnsw (embedding vector_cosine_ops)")?;

    // Every row must be indexed.
    let stats = db.storage.vector_indexes().get_index_stats("pbf_idx")?;
    assert_eq!(
        stats.num_vectors as i64, n,
        "parallel backfill must index all {} rows",
        n
    );

    // Searching for an exact copy of a known row must return that row first.
    let probe = synth_vec3(2500);
    let hits = db.storage.vector_indexes().search("pbf_idx", &probe.to_vec(), 1)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].0, 2500,
        "exact-match probe must find its own row via the parallel-built index"
    );
    Ok(())
}
