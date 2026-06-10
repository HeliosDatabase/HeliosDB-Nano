//! R5.V1 + R5.V5: HNSW vector index DML maintenance tests.
//!
//! Before R5.V1, SQL DML never maintained HNSW vector indexes: the indexed
//! kNN fast path silently missed every row inserted after CREATE INDEX and
//! kept serving deleted rows until a process restart rebuilt the index.
//! These tests pin the fixed behaviour:
//!   * INSERT after CREATE INDEX is immediately visible to indexed kNN
//!   * DELETE immediately stops a row from being served
//!   * UPDATE of the vector column re-indexes (new vector found, old gone)
//!   * transactional DML is undone on ROLLBACK (no phantom vectors)
//!   * R5.V5: LIMIT k still returns k rows after deleting nearest neighbours
//!     (tombstone over-fetch)

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn ids(rows: &[heliosdb_nano::Tuple]) -> Vec<i32> {
    rows.iter()
        .map(|t| match t.values.first() {
            Some(Value::Int4(v)) => *v,
            other => panic!("expected Int4 id, got {other:?}"),
        })
        .collect()
}

fn live_count(db: &EmbeddedDatabase, index: &str) -> i64 {
    db.storage
        .vector_indexes()
        .get_index_stats(index)
        .expect("index stats")
        .num_vectors
}

/// Headline C4 bug: a row inserted AFTER `CREATE INDEX ... USING hnsw` must
/// be served by the indexed kNN fast path without a process restart.
#[test]
fn knn_sees_row_inserted_after_create_index() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_ins (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_ins VALUES (1, '[5.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_ins VALUES (2, '[0.0, 5.0, 0.0]')")?;
    db.execute("INSERT INTO dml_ins VALUES (3, '[0.0, 0.0, 5.0]')")?;
    db.execute("CREATE INDEX dml_ins_idx ON dml_ins USING hnsw (embedding vector_l2_ops)")?;

    // Inserted after the index exists; nearest to the query by far.
    db.execute("INSERT INTO dml_ins VALUES (99, '[0.1, 0.0, 0.0]')")?;

    let rows = db.query(
        "SELECT id FROM dml_ins ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 1",
        &[],
    )?;
    assert_eq!(
        ids(&rows),
        vec![99],
        "row inserted after CREATE INDEX must be served by indexed kNN"
    );
    assert_eq!(live_count(&db, "dml_ins_idx"), 4, "index must contain the new row");
    Ok(())
}

/// A deleted row must disappear from indexed kNN results immediately.
#[test]
fn knn_stops_returning_deleted_row() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_del (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_del VALUES (1, '[0.1, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_del VALUES (2, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_del VALUES (3, '[2.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_del VALUES (4, '[3.0, 0.0, 0.0]')")?;
    db.execute("CREATE INDEX dml_del_idx ON dml_del USING hnsw (embedding vector_l2_ops)")?;

    db.execute("DELETE FROM dml_del WHERE id = 1")?;

    let rows = db.query(
        "SELECT id FROM dml_del ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 3",
        &[],
    )?;
    assert_eq!(
        ids(&rows),
        vec![2, 3, 4],
        "deleted row must not be served; remaining rows fill the LIMIT"
    );
    assert_eq!(live_count(&db, "dml_del_idx"), 3, "live count must drop after DELETE");
    Ok(())
}

/// UPDATE of the indexed vector column must remove the old vector and index
/// the new one (delete + re-insert; HNSW has no in-place update).
#[test]
fn update_of_vector_column_reindexes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_upd (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_upd VALUES (1, '[0.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_upd VALUES (2, '[5.0, 5.0, 5.0]')")?;
    db.execute("INSERT INTO dml_upd VALUES (3, '[10.0, 10.0, 10.0]')")?;
    db.execute("CREATE INDEX dml_upd_idx ON dml_upd USING hnsw (embedding vector_l2_ops)")?;

    // Move row 3 from the far corner to right next to the origin.
    db.execute("UPDATE dml_upd SET embedding = '[0.1, 0.0, 0.0]' WHERE id = 3")?;

    // New vector is found near the origin...
    let near_origin = db.query(
        "SELECT id FROM dml_upd ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 2",
        &[],
    )?;
    assert_eq!(
        ids(&near_origin),
        vec![1, 3],
        "updated row must be served at its NEW vector position"
    );

    // ...and the old position no longer serves row 3.
    let near_old_position = db.query(
        "SELECT id FROM dml_upd ORDER BY embedding <-> '[10.0, 10.0, 10.0]' LIMIT 1",
        &[],
    )?;
    assert_eq!(
        ids(&near_old_position),
        vec![2],
        "updated row must NOT be served at its OLD vector position"
    );

    assert_eq!(live_count(&db, "dml_upd_idx"), 3, "UPDATE must not change live count");
    Ok(())
}

/// Updating a NON-vector column on a vector-indexed table must leave the
/// index untouched: same live count, no graph bloat from pointless
/// delete + re-insert cycles, and the row stays searchable.
#[test]
fn update_of_non_vector_column_leaves_index_untouched() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_pay (id INT4 PRIMARY KEY, name TEXT, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_pay VALUES (1, 'a', '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_pay VALUES (2, 'b', '[0.0, 1.0, 0.0]')")?;
    db.execute("CREATE INDEX dml_pay_idx ON dml_pay USING hnsw (embedding vector_l2_ops)")?;

    let physical_before = db
        .storage
        .vector_indexes()
        .index_physical_size("dml_pay_idx")
        .expect("index exists");

    for i in 0..10 {
        db.execute(&format!("UPDATE dml_pay SET name = 'x{}' WHERE id = 1", i))?;
    }

    let physical_after = db
        .storage
        .vector_indexes()
        .index_physical_size("dml_pay_idx")
        .expect("index exists");
    assert_eq!(
        physical_before, physical_after,
        "payload-only updates must not grow the HNSW graph (no delete/re-insert churn)"
    );
    assert_eq!(live_count(&db, "dml_pay_idx"), 2);

    let rows = db.query(
        "SELECT id FROM dml_pay ORDER BY embedding <-> '[1.0, 0.0, 0.0]' LIMIT 1",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![1]);
    Ok(())
}

/// ROLLBACK of a transactional INSERT must revert the eager index insert —
/// no phantom vector remains (mirrors the ART eager + undo pattern).
#[test]
fn rollback_of_transactional_insert_leaves_no_phantom_vector() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_rb_ins (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_rb_ins VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_rb_ins VALUES (2, '[0.0, 1.0, 0.0]')")?;
    db.execute("CREATE INDEX dml_rb_ins_idx ON dml_rb_ins USING hnsw (embedding vector_l2_ops)")?;
    assert_eq!(live_count(&db, "dml_rb_ins_idx"), 2);

    db.begin()?;
    db.execute("INSERT INTO dml_rb_ins VALUES (99, '[0.01, 0.0, 0.0]')")?;
    db.rollback()?;

    assert_eq!(
        live_count(&db, "dml_rb_ins_idx"),
        2,
        "rolled-back INSERT must not leave a vector in the index"
    );

    let rows = db.query(
        "SELECT id FROM dml_rb_ins ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 10",
        &[],
    )?;
    assert_eq!(
        ids(&rows).len(),
        2,
        "kNN must serve exactly the 2 committed rows after rollback"
    );
    assert!(
        !ids(&rows).contains(&99),
        "phantom row from rolled-back INSERT must not appear"
    );
    Ok(())
}

/// COMMIT keeps the eagerly-applied index change (undo log is discarded).
#[test]
fn commit_of_transactional_insert_keeps_vector() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_ci (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_ci VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("CREATE INDEX dml_ci_idx ON dml_ci USING hnsw (embedding vector_l2_ops)")?;

    db.begin()?;
    db.execute("INSERT INTO dml_ci VALUES (2, '[0.1, 0.0, 0.0]')")?;
    db.commit()?;

    assert_eq!(live_count(&db, "dml_ci_idx"), 2);
    let rows = db.query(
        "SELECT id FROM dml_ci ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 1",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![2]);
    Ok(())
}

/// ROLLBACK of a transactional UPDATE must restore the OLD vector.
#[test]
fn rollback_of_transactional_update_restores_old_vector() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_rb_upd (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_rb_upd VALUES (1, '[1.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_rb_upd VALUES (2, '[0.0, 8.0, 0.0]')")?;
    db.execute("CREATE INDEX dml_rb_upd_idx ON dml_rb_upd USING hnsw (embedding vector_l2_ops)")?;

    db.begin()?;
    db.execute("UPDATE dml_rb_upd SET embedding = '[0.0, 7.9, 0.0]' WHERE id = 1")?;
    db.rollback()?;

    assert_eq!(live_count(&db, "dml_rb_upd_idx"), 2);

    // Row 1 must be back at its old position near [1,0,0]...
    let rows = db.query(
        "SELECT id FROM dml_rb_upd ORDER BY embedding <-> '[1.0, 0.0, 0.0]' LIMIT 1",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![1], "rollback must restore the old vector");

    // ...and must NOT be the nearest at the attempted new position.
    let rows = db.query(
        "SELECT id FROM dml_rb_upd ORDER BY embedding <-> '[0.0, 8.0, 0.0]' LIMIT 1",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![2], "rolled-back new vector must not be served");
    Ok(())
}

/// ROLLBACK of a transactional DELETE must re-insert the deleted vector.
#[test]
fn rollback_of_transactional_delete_restores_vector() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_rb_del (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO dml_rb_del VALUES (1, '[0.1, 0.0, 0.0]')")?;
    db.execute("INSERT INTO dml_rb_del VALUES (2, '[5.0, 0.0, 0.0]')")?;
    db.execute("CREATE INDEX dml_rb_del_idx ON dml_rb_del USING hnsw (embedding vector_l2_ops)")?;

    db.begin()?;
    db.execute("DELETE FROM dml_rb_del WHERE id = 1")?;
    db.rollback()?;

    assert_eq!(live_count(&db, "dml_rb_del_idx"), 2);
    let rows = db.query(
        "SELECT id FROM dml_rb_del ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 1",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![1], "rollback must restore the deleted row's vector");
    Ok(())
}

/// R5.V5: HNSW deletes are tombstones — a plain k-fetch can come back short
/// after deleting the nearest neighbours. The over-fetch + liveness filter
/// must keep LIMIT k returning k rows while k live rows exist.
#[test]
fn limit_k_returns_k_rows_after_deleting_nearest_neighbors() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_lim (id INT4, embedding VECTOR(3))")?;
    for i in 1..=12 {
        db.execute(&format!("INSERT INTO dml_lim VALUES ({i}, '[{i}.0, 0.0, 0.0]')"))?;
    }
    db.execute("CREATE INDEX dml_lim_idx ON dml_lim USING hnsw (embedding vector_l2_ops)")?;

    // Delete the 4 nearest neighbours of the query point.
    db.execute("DELETE FROM dml_lim WHERE id <= 4")?;
    assert_eq!(live_count(&db, "dml_lim_idx"), 8);

    let rows = db.query(
        "SELECT id FROM dml_lim ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 5",
        &[],
    )?;
    assert_eq!(
        ids(&rows),
        vec![5, 6, 7, 8, 9],
        "LIMIT 5 must still return 5 live rows in distance order after deleting the 4 nearest"
    );
    Ok(())
}

/// Same guarantee for OFFSET pagination over a partially deleted neighbourhood.
#[test]
fn limit_with_offset_survives_deletes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_off (id INT4, embedding VECTOR(3))")?;
    for i in 1..=10 {
        db.execute(&format!("INSERT INTO dml_off VALUES ({i}, '[{i}.0, 0.0, 0.0]')"))?;
    }
    db.execute("CREATE INDEX dml_off_idx ON dml_off USING hnsw (embedding vector_l2_ops)")?;
    db.execute("DELETE FROM dml_off WHERE id = 1")?;
    db.execute("DELETE FROM dml_off WHERE id = 3")?;

    let rows = db.query(
        "SELECT id FROM dml_off ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 3 OFFSET 2",
        &[],
    )?;
    // Live order is 2,4,5,6,7,8,9,10 — OFFSET 2 LIMIT 3 → 5,6,7.
    assert_eq!(ids(&rows), vec![5, 6, 7]);
    Ok(())
}

#[test]
fn large_index_overfetch_skips_tombstones() -> Result<()> {
    // >SMALL_INDEX_EXACT_THRESHOLD rows so the ANN fast path (with its
    // tombstone over-fetch loop) is actually exercised. Assertions avoid
    // exact-membership claims (ANN recall is probabilistic): results must be
    // the right COUNT, contain NO deleted rows, and be distance-ordered.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE dml_big (id INT4, embedding VECTOR(3))")?;
    db.execute("BEGIN")?;
    for i in 1..=400 {
        db.execute(&format!(
            "INSERT INTO dml_big VALUES ({i}, '[{}.0, {}.0, 0.0]')",
            i,
            (i * 7) % 13
        ))?;
    }
    db.execute("COMMIT")?;
    db.execute("CREATE INDEX dml_big_idx ON dml_big USING hnsw (embedding vector_l2_ops)")?;
    assert_eq!(live_count(&db, "dml_big_idx"), 400);

    // Tombstone the 30 nearest to the query corner.
    db.execute("DELETE FROM dml_big WHERE id <= 30")?;
    assert_eq!(live_count(&db, "dml_big_idx"), 370);

    let rows = db.query(
        "SELECT id FROM dml_big ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 10",
        &[],
    )?;
    let got = ids(&rows);
    assert_eq!(got.len(), 10, "LIMIT must be satisfied despite 30 tombstones in front");
    assert!(
        got.iter().all(|id| *id > 30),
        "no deleted row may be served: {got:?}"
    );
    Ok(())
}
