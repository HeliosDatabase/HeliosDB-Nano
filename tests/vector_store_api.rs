use std::collections::HashMap;

use heliosdb_nano::{EmbeddedDatabase, Result};

#[test]
fn vector_store_preserves_ids_metadata_namespace_and_fetches() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let store = db.create_vector_store_with_options("docs", 3, "cosine", "hnsw")?;
    assert_eq!(store.index_type, "hnsw");

    let mut meta_a = HashMap::new();
    meta_a.insert("tenant".to_string(), serde_json::json!("acme"));
    meta_a.insert("kind".to_string(), serde_json::json!("guide"));
    let mut meta_b = HashMap::new();
    meta_b.insert("tenant".to_string(), serde_json::json!("acme"));
    meta_b.insert("kind".to_string(), serde_json::json!("ticket"));

    let ids = db.insert_vectors_with_options(
        "docs",
        Some(vec!["a".to_string(), "b".to_string()]),
        vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
        Some(vec![meta_a.clone(), meta_b]),
        Some("prod".to_string()),
    )?;
    assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);

    let fetched = db.fetch_vector_records("docs", vec!["a".to_string()], Some("prod"))?;
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].id, "a");
    assert_eq!(fetched[0].metadata.as_ref(), Some(&meta_a));

    let hidden = db.fetch_vector_records("docs", vec!["a".to_string()], Some("dev"))?;
    assert!(hidden.is_empty());
    Ok(())
}

#[test]
fn vector_store_search_applies_metadata_filter_before_top_k() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.create_vector_store_with_options("docs", 2, "euclidean", "hnsw")?;

    let mut keep = HashMap::new();
    keep.insert("team".to_string(), serde_json::json!("search"));
    let mut skip = HashMap::new();
    skip.insert("team".to_string(), serde_json::json!("billing"));

    db.insert_vectors_with_options(
        "docs",
        Some(vec!["keep".to_string(), "skip".to_string()]),
        vec![vec![0.0, 1.0], vec![0.0, 0.1]],
        Some(vec![keep.clone(), skip]),
        Some("ns".to_string()),
    )?;

    let results = db.search_vectors_with_options("docs", vec![0.0, 0.0], 1, Some(&keep), Some("ns"))?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "keep");
    assert_eq!(results[0].metadata.as_ref(), Some(&keep));
    assert!(results[0].vector.is_some());
    Ok(())
}

#[test]
fn vector_delete_removes_from_unfiltered_search() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.create_vector_store_with_options("docs", 2, "euclidean", "hnsw")?;
    db.insert_vectors_with_options("docs", Some(vec!["a".to_string()]), vec![vec![0.0, 0.0]], None, None)?;

    let deleted = db.delete_vectors_in_namespace("docs", vec!["a".to_string()], None)?;
    assert_eq!(deleted, 1);
    assert!(db
        .search_vectors_with_options("docs", vec![0.0, 0.0], 10, None, None)?
        .is_empty());
    assert_eq!(db.get_vector_store("docs")?.vector_count, 0);
    Ok(())
}

#[test]
fn vector_upsert_replaces_single_visible_record() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.create_vector_store_with_options("docs", 2, "euclidean", "hnsw")?;
    db.insert_vectors_with_options("docs", Some(vec!["a".to_string()]), vec![vec![0.0, 0.0]], None, None)?;

    db.upsert_vectors_with_options("docs", vec!["a".to_string()], vec![vec![10.0, 10.0]], None, None)?;

    let old_hits = db.search_vectors_with_options("docs", vec![0.0, 0.0], 10, None, None)?;
    assert!(old_hits.iter().all(|hit| hit.id != "vec_1"));
    let fetched = db.fetch_vectors("docs", vec!["a".to_string()])?;
    assert_eq!(fetched, vec![("a".to_string(), vec![10.0, 10.0])]);
    assert_eq!(db.get_vector_store("docs")?.vector_count, 1);
    Ok(())
}

#[test]
fn vector_namespace_scopes_external_ids() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.create_vector_store_with_options("docs", 2, "euclidean", "hnsw")?;
    db.insert_vectors_with_options(
        "docs",
        Some(vec!["same".to_string()]),
        vec![vec![0.0, 0.0]],
        None,
        Some("prod".to_string()),
    )?;
    db.insert_vectors_with_options(
        "docs",
        Some(vec!["same".to_string()]),
        vec![vec![5.0, 5.0]],
        None,
        Some("dev".to_string()),
    )?;

    let prod = db.fetch_vector_records("docs", vec!["same".to_string()], Some("prod"))?;
    let dev = db.fetch_vector_records("docs", vec!["same".to_string()], Some("dev"))?;
    assert_eq!(prod[0].vector, vec![0.0, 0.0]);
    assert_eq!(dev[0].vector, vec![5.0, 5.0]);

    let deleted = db.delete_vectors_in_namespace("docs", vec!["same".to_string()], Some("prod"))?;
    assert_eq!(deleted, 1);
    assert!(db
        .fetch_vector_records("docs", vec!["same".to_string()], Some("prod"))?
        .is_empty());
    assert_eq!(
        db.fetch_vector_records("docs", vec!["same".to_string()], Some("dev"))?[0].vector,
        vec![5.0, 5.0],
    );
    Ok(())
}

#[test]
fn empty_store_fetch_delete_filtered_search_are_empty() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.create_vector_store_with_options("docs", 2, "euclidean", "hnsw")?;
    let mut filter = HashMap::new();
    filter.insert("team".to_string(), serde_json::json!("search"));

    assert!(db.fetch_vectors("docs", vec!["missing".to_string()])?.is_empty());
    assert_eq!(
        db.delete_vectors_in_namespace("docs", vec!["missing".to_string()], None)?,
        0
    );
    assert!(db
        .search_vectors_with_options("docs", vec![0.0, 0.0], 5, Some(&filter), None)?
        .is_empty());
    Ok(())
}

#[test]
fn vector_dimension_mismatch_is_rejected_for_vector_literals() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, embedding VECTOR(3))")?;
    let err = db.execute("INSERT INTO docs VALUES (1, ARRAY[0.1, 0.2])");
    assert!(err.is_err(), "VECTOR(3) should reject a two-dimensional vector literal");
    Ok(())
}

#[test]
fn multi_precision_vector_type_aliases_parse_as_vectors() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE docs (
        id INT PRIMARY KEY,
        a VECTOR_F16(3),
        b VECTOR_I8(3),
        c VECTOR_I16(3),
        d HALFVEC(3)
    )",
    )?;
    db.execute(
        "INSERT INTO docs VALUES (
        1,
        ARRAY[0.1, 0.2, 0.3],
        ARRAY[0.1, 0.2, 0.3],
        ARRAY[0.1, 0.2, 0.3],
        ARRAY[0.1, 0.2, 0.3]
    )",
    )?;
    Ok(())
}

#[test]
fn persistent_vector_store_reports_feature_gate_without_vector_persist() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let result = db.create_vector_store_with_options("docs", 3, "cosine", "persistent_hnsw");

    #[cfg(feature = "vector-persist")]
    assert!(result.is_ok());

    #[cfg(not(feature = "vector-persist"))]
    {
        let message = result
            .expect_err("persistent index should be feature-gated")
            .to_string();
        assert!(message.contains("vector-persist"), "{message}");
    }

    Ok(())
}
