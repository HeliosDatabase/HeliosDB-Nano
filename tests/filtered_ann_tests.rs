//! R5.V4: filtered ANN — kNN fast path with a WHERE clause.
//!
//! Before R5.V4, any WHERE clause disqualified `try_vector_knn_topk` (the
//! matcher only accepted a bare `Project(Sort(Scan))`) and the query fell
//! back to a brute-force O(n) scan+sort. R5.V4 extends the matcher to
//! `Project(Sort(Filter(Scan)))` / `Project(Sort(FilteredScan))` for simple
//! predicates (column-vs-constant comparisons joined by AND) and answers
//! them as post-filtered ANN: over-fetch candidates from the HNSW index,
//! load the rows, evaluate the predicate, keep k — escalating the fetch
//! width while matches run short and candidates remain.
//!
//! Every test compares the fast path against the brute-force path (forced
//! via the `HELIOS_KNN_FAST_OFF` kill switch) on an index large enough
//! (> 256 live vectors) to clear the small-index exact-scan fallback, so
//! the ANN machinery is actually engaged.
//!
//! The data is constructed so distances to the query point are strictly
//! ordered by id (d^2 = id^2 + noise^2 with noise^2 < 0.5 and consecutive
//! id^2 gaps >= 3): ties are impossible and both paths must produce the
//! exact same id sequence.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};
use std::sync::Mutex;

/// Serializes tests in this file: the `HELIOS_KNN_FAST_OFF` kill switch is
/// process-global env state, so tests must not interleave around it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const N_ROWS: usize = 1200;
const QUERY: &str = "'[0.0, 0.0, 0.0, 0.0]'";

fn ids(rows: &[Tuple]) -> Vec<i32> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::Int4(v)) => *v,
            Some(Value::Int8(v)) => *v as i32,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect()
}

/// Deterministic per-row noise in [0.0, 0.7): keeps d^2 strictly ordered by
/// id while making the vectors non-degenerate for the HNSW graph.
fn noise(i: usize, salt: usize) -> f32 {
    (((i * 31 + salt * 17) % 100) as f32) * 0.007
}

/// `items(id, category, val, embedding VECTOR(4))` with `N_ROWS` rows:
///   * embedding = [id, n1, n2, 0] — distance to the origin strictly
///     ascending in id
///   * category  = 'even' / 'odd' by id parity, NULL every 97th row
///   * val       = id
/// The index is created BEFORE the inserts so the graph is built
/// incrementally through the R5.V1 DML hooks: the parallel bulk build used
/// for CREATE INDEX on a populated table produces measurably worse recall
/// (the small-index exact fallback exists for that reason), which would
/// make exact fast-vs-brute equivalence assertions flaky.
fn seed(db: &EmbeddedDatabase, n: usize) -> Result<()> {
    db.execute("CREATE TABLE items (id INT4, category TEXT, val INT4, embedding VECTOR(4))")?;
    db.execute("CREATE INDEX items_emb_idx ON items USING hnsw (embedding vector_l2_ops)")?;
    let mut batch: Vec<String> = Vec::with_capacity(200);
    for i in 0..n {
        let category = if i % 97 == 0 {
            "NULL".to_string()
        } else if i % 2 == 0 {
            "'even'".to_string()
        } else {
            "'odd'".to_string()
        };
        batch.push(format!(
            "({i}, {category}, {i}, '[{}.0, {:.3}, {:.3}, 0.0]')",
            i,
            noise(i, 1),
            noise(i, 2)
        ));
        if batch.len() == 200 || i == n - 1 {
            db.execute(&format!("INSERT INTO items VALUES {}", batch.join(", ")))?;
            batch.clear();
        }
    }
    Ok(())
}

/// Runs `query` twice — once on the fast path, once with the kill switch
/// forcing the brute-force scan+sort — and returns both id sequences.
fn fast_vs_brute(db: &EmbeddedDatabase, query: &str, params: &[Value]) -> Result<(Vec<i32>, Vec<i32>)> {
    assert!(
        std::env::var_os("HELIOS_KNN_FAST_OFF").is_none(),
        "kill switch leaked from a previous test"
    );
    let fast = db.query_params(query, params)?;
    std::env::set_var("HELIOS_KNN_FAST_OFF", "1");
    let brute = db.query_params(query, params);
    std::env::remove_var("HELIOS_KNN_FAST_OFF");
    Ok((ids(&fast), ids(&brute?)))
}

/// Moderate selectivity (~50%): the first over-fetch round already contains
/// k matches. Also covers OFFSET and NULL categories being excluded by the
/// equality predicate on both paths.
#[test]
fn filtered_knn_matches_brute_force_moderate_selectivity() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    let q = format!("SELECT id FROM items WHERE category = 'even' ORDER BY embedding <-> {QUERY} LIMIT 10");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "filtered kNN != brute force");
    // Nearest even-category ids ascending, skipping NULL-category id 0.
    assert_eq!(fast, vec![2, 4, 6, 8, 10, 12, 14, 16, 18, 20]);

    let q = format!("SELECT id FROM items WHERE category = 'even' ORDER BY embedding <-> {QUERY} LIMIT 10 OFFSET 5");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "filtered kNN with OFFSET != brute force");
    assert_eq!(fast, vec![12, 14, 16, 18, 20, 22, 24, 26, 28, 30]);
    Ok(())
}

/// Low selectivity (< k matches in the whole table): escalation reaches the
/// index's physical size and the (correct) result is shorter than LIMIT.
#[test]
fn filtered_knn_fewer_than_k_matches_returns_all_matches() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    // Exactly 5 rows match; k = 10.
    let q = format!("SELECT id FROM items WHERE val >= 1195 ORDER BY embedding <-> {QUERY} LIMIT 10");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "low-selectivity filtered kNN != brute force");
    assert_eq!(fast, vec![1195, 1196, 1197, 1198, 1199]);
    Ok(())
}

/// The k nearest candidates ALL fail the filter (matching rows are the
/// farthest fifth of the table): the over-fetch escalates geometrically
/// and — because a full-size graph search would not be exact — ends by
/// handing the query back to the brute-force path. Results must equal
/// brute force exactly.
#[test]
fn filtered_knn_escalates_when_nearest_candidates_fail_filter() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    let q = format!("SELECT id FROM items WHERE val >= 1000 ORDER BY embedding <-> {QUERY} LIMIT 10");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "escalating filtered kNN != brute force");
    assert_eq!(fast, (1000..1010).collect::<Vec<i32>>());
    Ok(())
}

/// AND of two comparisons with bound parameters — the canonical driver
/// idiom `WHERE category = $2 AND val < $3 ORDER BY emb <-> $1 LIMIT k`.
#[test]
fn filtered_knn_and_predicate_with_parameters() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    let params = vec![
        Value::Vector(vec![0.0, 0.0, 0.0, 0.0]),
        Value::String("odd".to_string()),
        Value::Int4(500),
    ];
    let q = "SELECT id FROM items WHERE category = $2 AND val < $3 ORDER BY embedding <-> $1 LIMIT 10";
    let (fast, brute) = fast_vs_brute(&db, q, &params)?;
    assert_eq!(fast, brute, "parameterized AND-filtered kNN != brute force");
    assert_eq!(fast, vec![1, 3, 5, 7, 9, 11, 13, 15, 17, 19]);
    Ok(())
}

/// Filter on a column that carries its own ART secondary index: whatever
/// plan shape the optimizer picks (index probe or kNN fast path), results
/// must match brute force.
#[test]
fn filtered_knn_on_art_indexed_column_still_correct() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;
    db.execute("CREATE INDEX items_val_idx ON items (val)")?;

    let q = format!("SELECT id FROM items WHERE val = 777 ORDER BY embedding <-> {QUERY} LIMIT 10");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "kNN filtered on ART-indexed column != brute force");
    assert_eq!(fast, vec![777]);
    Ok(())
}

/// Predicates the fast path must NOT claim (OR, column-vs-column) still
/// produce correct results through the brute-force fallback.
#[test]
fn complex_predicates_fall_back_and_stay_correct() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    let q = format!("SELECT id FROM items WHERE val < 5 OR val >= 1198 ORDER BY embedding <-> {QUERY} LIMIT 10");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "OR-filtered kNN != brute force");
    assert_eq!(fast, vec![0, 1, 2, 3, 4, 1198, 1199]);

    let q = format!("SELECT id FROM items WHERE id = val ORDER BY embedding <-> {QUERY} LIMIT 3");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "column-vs-column filtered kNN != brute force");
    assert_eq!(fast, vec![0, 1, 2]);
    Ok(())
}

/// R2.3 gate: inside a ReadCommitted txn with no staged writes the filtered
/// fast path may serve; once the txn writes the table, reads must take the
/// slow path and see the staged row (read-your-writes).
#[test]
fn txn_filtered_knn_respects_r23_gate() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    let knn = format!("SELECT id FROM items WHERE category = 'odd' ORDER BY embedding <-> {QUERY} LIMIT 3");
    let autocommit = ids(&db.query(&knn, &[])?);
    assert_eq!(autocommit, vec![1, 3, 5]);

    let session = db.create_wire_session("r5v4")?;
    db.execute_for_session(session, "BEGIN")?;

    // No staged writes: fast path allowed, identical to autocommit.
    let (rows, _) = db.query_with_columns_for_session(session, &knn)?;
    assert_eq!(ids(&rows), autocommit, "in-txn filtered kNN != autocommit");

    // Stage a matching nearest row INSIDE the txn: the gate must force the
    // slow path so the staged row is visible (the HNSW index only reflects
    // it at commit).
    db.execute_for_session(
        session,
        "INSERT INTO items VALUES (9999, 'odd', 9999, '[0.5, 0.0, 0.0, 0.0]')",
    )?;
    let (rows, _) = db.query_with_columns_for_session(session, &knn)?;
    assert_eq!(
        ids(&rows),
        vec![9999, 1, 3],
        "filtered kNN must see the txn's own staged row"
    );

    db.execute_for_session(session, "ROLLBACK")?;
    db.destroy_session(session)?;

    // After rollback the staged row is gone from both paths.
    let (fast, brute) = fast_vs_brute(&db, &knn, &[])?;
    assert_eq!(fast, brute);
    assert_eq!(fast, vec![1, 3, 5]);
    Ok(())
}

/// Unfiltered kNN with a LIMIT beyond the graph search's saturation point:
/// hnsw_rs's beam can stop yielding new candidates well below the requested
/// k on unfavourable topologies (observed ~48 on this line-shaped data).
/// The fast path used to return the truncated set silently; the live-count
/// guard must hand such queries to the brute-force path so the full LIMIT
/// comes back.
#[test]
fn unfiltered_knn_large_limit_not_truncated_by_search_saturation() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    let q = format!("SELECT id FROM items ORDER BY embedding <-> {QUERY} LIMIT 100");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast.len(), 100, "LIMIT 100 must return 100 of {N_ROWS} live rows");
    assert_eq!(fast, brute, "large-LIMIT kNN != brute force");
    assert_eq!(fast, (0..100).collect::<Vec<i32>>());
    Ok(())
}

/// DML after CREATE INDEX (R5.V1 maintenance) composes with the filter:
/// inserted rows show up, deleted rows disappear — on both paths.
#[test]
fn filtered_knn_composes_with_dml_maintenance() -> Result<()> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db, N_ROWS)?;

    db.execute("INSERT INTO items VALUES (8888, 'even', 8888, '[0.25, 0.0, 0.0, 0.0]')")?;
    db.execute("DELETE FROM items WHERE id = 2")?;

    let q = format!("SELECT id FROM items WHERE category = 'even' ORDER BY embedding <-> {QUERY} LIMIT 5");
    let (fast, brute) = fast_vs_brute(&db, &q, &[])?;
    assert_eq!(fast, brute, "filtered kNN after DML != brute force");
    assert_eq!(fast, vec![8888, 4, 6, 8, 10]);
    Ok(())
}
