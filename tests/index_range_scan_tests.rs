//! R4.4 index range scan tests.
//!
//! Correctness is checked against a "row twin": an identical table with NO
//! secondary index, so the same query runs through the generic scan + filter
//! path. Any divergence is a fast-path bug.

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

/// Render a result set order-insensitively for comparison.
fn rows_sorted(rows: &[Tuple]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|t| t.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("|"))
        .collect();
    out.sort();
    out
}

/// Render a result set order-SENSITIVELY (for ORDER BY checks).
fn rows_exact(rows: &[Tuple]) -> Vec<String> {
    rows.iter()
        .map(|t| t.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("|"))
        .collect()
}

/// Build `indexed` (with secondary indexes) and `twin` (no indexes) with
/// identical contents: ints incl. negatives, text, floats, and NULLs.
fn build_pair(db: &EmbeddedDatabase) {
    for table in ["rt_indexed", "rt_twin"] {
        db.execute(&format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, score INT, name TEXT, ratio FLOAT8)"
        ))
        .unwrap();
        let mut i = 0;
        for score in (-50..150).step_by(1) {
            let name = if score % 17 == 0 {
                "NULL".to_string()
            } else {
                format!("'name{:03}'", (score + 50) % 90)
            };
            let ratio = if score % 23 == 0 {
                "NULL".to_string()
            } else {
                format!("{}", f64::from(score) / 7.0)
            };
            let score_sql = if score % 13 == 0 {
                "NULL".to_string()
            } else {
                score.to_string()
            };
            db.execute(&format!(
                "INSERT INTO {table} VALUES ({i}, {score_sql}, {name}, {ratio})"
            ))
            .unwrap();
            i += 1;
        }
    }
    db.execute("CREATE INDEX idx_rt_score ON rt_indexed(score)").unwrap();
    db.execute("CREATE INDEX idx_rt_name ON rt_indexed(name)").unwrap();
    db.execute("CREATE INDEX idx_rt_ratio ON rt_indexed(ratio)").unwrap();
}

fn assert_twin_match(db: &EmbeddedDatabase, predicate: &str) {
    let indexed = db
        .query(
            &format!("SELECT id, score, name FROM rt_indexed WHERE {predicate}"),
            &[],
        )
        .unwrap();
    let twin = db
        .query(&format!("SELECT id, score, name FROM rt_twin WHERE {predicate}"), &[])
        .unwrap();
    assert_eq!(
        rows_sorted(&indexed),
        rows_sorted(&twin),
        "index range scan diverged from full scan for predicate: {predicate}"
    );
    assert!(
        !twin.is_empty() || indexed.is_empty(),
        "twin sanity check for predicate: {predicate}"
    );
}

#[test]
fn range_predicates_match_full_scan_twin() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    // Inclusive / exclusive bounds, negatives (sign-flip encoding), BETWEEN,
    // combined bounds, bounds on text and float columns, reversed operands,
    // residual conjuncts — NULLs must never leak into the result.
    let predicates = [
        "score > 100",
        "score >= 100",
        "score < -25",
        "score <= -25",
        "score > -10 AND score < 10",
        "score >= -10 AND score <= 10",
        "score BETWEEN -5 AND 5",
        "score BETWEEN 120 AND 500",
        "score > 9999",
        "50 < score",
        "50 >= score",
        "score > 10 AND score > 40",
        "score < 60 AND score <= 55",
        "score > 0 AND id < 100",
        "score > 0 AND name = 'name050'",
        "name > 'name080'",
        "name >= 'name080' AND name < 'name085'",
        "name BETWEEN 'name000' AND 'name010'",
        "ratio > 10.0",
        "ratio >= -3.5 AND ratio < 3.5",
        "ratio < 0.0",
    ];
    for predicate in predicates {
        assert_twin_match(&db, predicate);
    }
}

#[test]
fn range_scan_excludes_nulls_like_full_scan() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    // Unbounded-one-side ranges over columns containing NULLs.
    for predicate in ["score < 1000", "score > -1000", "name < 'zzz'", "ratio > -1000.0"] {
        assert_twin_match(&db, predicate);
        let rows = db
            .query(
                &format!("SELECT score, name, ratio FROM rt_indexed WHERE {predicate}"),
                &[],
            )
            .unwrap();
        // No NULL may satisfy a range predicate.
        let col = if predicate.starts_with("score") {
            0
        } else if predicate.starts_with("name") {
            1
        } else {
            2
        };
        assert!(
            rows.iter().all(|t| !matches!(t.values.get(col), Some(Value::Null))),
            "NULL leaked through range predicate: {predicate}"
        );
    }
}

#[test]
fn range_scan_tracks_dml() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    for table in ["rt_indexed", "rt_twin"] {
        db.execute(&format!("UPDATE {table} SET score = 500 WHERE id = 10"))
            .unwrap();
        db.execute(&format!("DELETE FROM {table} WHERE id = 11")).unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES (9001, 501, 'late', 1.0)"))
            .unwrap();
    }
    assert_twin_match(&db, "score >= 500");
    assert_twin_match(&db, "score > -100 AND score < 0");
}

#[test]
fn order_by_indexed_column_limit_matches_twin() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    // ORDER BY ASC + LIMIT must produce the exact ordered prefix the generic
    // sort path produces (engine semantics: NULLS FIRST on ASC), including
    // OFFSET and WHERE-bounded variants.
    let queries = [
        "SELECT id, score FROM {T} ORDER BY score LIMIT 5",
        "SELECT id, score FROM {T} ORDER BY score ASC LIMIT 25",
        "SELECT id, score FROM {T} ORDER BY score LIMIT 10 OFFSET 20",
        "SELECT id, score FROM {T} WHERE score > 50 ORDER BY score LIMIT 7",
        "SELECT id, score FROM {T} WHERE score BETWEEN -20 AND 20 ORDER BY score LIMIT 12",
        "SELECT id, name FROM {T} ORDER BY name LIMIT 9",
        "SELECT id, ratio FROM {T} ORDER BY ratio LIMIT 6",
    ];
    for query in queries {
        let indexed = db.query(&query.replace("{T}", "rt_indexed"), &[]).unwrap();
        let twin = db.query(&query.replace("{T}", "rt_twin"), &[]).unwrap();
        // Sort keys can tie (duplicate scores / NULLs); compare the sort-key
        // sequence exactly and the full row sets order-insensitively.
        let key_seq =
            |rows: &[Tuple]| -> Vec<String> { rows.iter().map(|t| format!("{:?}", t.values.get(1))).collect() };
        assert_eq!(
            key_seq(&indexed),
            key_seq(&twin),
            "sort-key order diverged for: {query}"
        );
        assert_eq!(
            rows_sorted(&indexed),
            rows_sorted(&twin),
            "row set diverged for: {query}"
        );
    }
}

#[test]
fn order_by_desc_still_correct() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    // DESC is NOT served by the ordered-index fast path — it must fall back
    // to the generic sort and stay correct.
    let indexed = db
        .query("SELECT id, score FROM rt_indexed ORDER BY score DESC LIMIT 10", &[])
        .unwrap();
    let twin = db
        .query("SELECT id, score FROM rt_twin ORDER BY score DESC LIMIT 10", &[])
        .unwrap();
    let key_seq = |rows: &[Tuple]| -> Vec<String> { rows.iter().map(|t| format!("{:?}", t.values.get(1))).collect() };
    assert_eq!(key_seq(&indexed), key_seq(&twin));
}

#[test]
fn order_by_limit_nulls_beyond_first_batch_still_sort_first() {
    // Edge case for the ordered-index top-k: empty-string keys ([]) sort
    // BEFORE the NULL key ([0x00]) in the index, so the first fetch batch can
    // fill the LIMIT with ""-rows while NULL rows — which the engine's ASC
    // semantics put FIRST in the output — are still unfetched. The fast path
    // must keep fetching until the NULL region is provably complete.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for table in ["nb_indexed", "nb_twin"] {
        db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, name TEXT)"))
            .unwrap();
        let mut id = 0;
        for _ in 0..15 {
            db.execute(&format!("INSERT INTO {table} VALUES ({id}, '')")).unwrap();
            id += 1;
        }
        for _ in 0..12 {
            db.execute(&format!("INSERT INTO {table} VALUES ({id}, NULL)")).unwrap();
            id += 1;
        }
        for _ in 0..10 {
            db.execute(&format!("INSERT INTO {table} VALUES ({id}, 'zzz')"))
                .unwrap();
            id += 1;
        }
    }
    db.execute("CREATE INDEX idx_nb_name ON nb_indexed(name)").unwrap();

    for sql in [
        "SELECT id, name FROM {T} ORDER BY name LIMIT 10",
        "SELECT id, name FROM {T} ORDER BY name LIMIT 20",
        "SELECT id, name FROM {T} ORDER BY name LIMIT 10 OFFSET 8",
    ] {
        let indexed = db.query(&sql.replace("{T}", "nb_indexed"), &[]).unwrap();
        let twin = db.query(&sql.replace("{T}", "nb_twin"), &[]).unwrap();
        let key_seq =
            |rows: &[Tuple]| -> Vec<String> { rows.iter().map(|t| format!("{:?}", t.values.get(1))).collect() };
        assert_eq!(key_seq(&indexed), key_seq(&twin), "sort-key order diverged for: {sql}");
    }
    // With 12 NULLs present, LIMIT 10 must be entirely NULL-keyed rows.
    let rows = db
        .query("SELECT name FROM nb_indexed ORDER BY name LIMIT 10", &[])
        .unwrap();
    assert!(
        rows.iter().all(|t| matches!(t.values.first(), Some(Value::Null))),
        "NULLS FIRST: the top 10 of 12 NULLs must all be NULL, got {rows:?}"
    );
}

#[test]
fn mixed_type_range_predicate_behaves_like_full_scan() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    // A bound that cannot be coerced to the column type must NOT be served
    // by the index fast path; whatever the generic path does (error or
    // empty/typed result), the indexed table must do the same.
    for predicate in ["score > 'abc'", "name > 42"] {
        let indexed = db.query(&format!("SELECT id FROM rt_indexed WHERE {predicate}"), &[]);
        let twin = db.query(&format!("SELECT id FROM rt_twin WHERE {predicate}"), &[]);
        match (indexed, twin) {
            (Ok(a), Ok(b)) => assert_eq!(rows_sorted(&a), rows_sorted(&b), "predicate: {predicate}"),
            (Err(_), Err(_)) => {} // both error: consistent ("errors cleanly")
            (a, b) => panic!(
                "indexed and twin disagreed on mixed-type predicate {predicate}: indexed={:?} twin={:?}",
                a.map(|r| r.len()),
                b.map(|r| r.len())
            ),
        }
    }
}

#[test]
fn parameterized_range_uses_index_and_matches_twin() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    let indexed = db
        .query_params(
            "SELECT id, score FROM rt_indexed WHERE score >= $1 AND score < $2",
            &[Value::Int4(-10), Value::Int4(40)],
        )
        .unwrap();
    let twin = db
        .query_params(
            "SELECT id, score FROM rt_twin WHERE score >= $1 AND score < $2",
            &[Value::Int4(-10), Value::Int4(40)],
        )
        .unwrap();
    assert_eq!(rows_sorted(&indexed), rows_sorted(&twin));
    assert!(!indexed.is_empty());
}

#[test]
fn explain_shows_index_range_scan() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    let plan = db
        .query(
            "EXPLAIN SELECT id FROM rt_indexed WHERE score > 120 AND score < 135",
            &[],
        )
        .unwrap();
    let text = rows_exact(&plan).join("\n");
    assert!(
        text.contains("Index Range Scan using idx_rt_score"),
        "EXPLAIN must surface the index range scan, got:\n{text}"
    );
    assert!(
        text.contains("score > 120"),
        "EXPLAIN must show the bounds, got:\n{text}"
    );

    // The twin (no index) must NOT claim a range scan.
    let plan = db
        .query("EXPLAIN SELECT id FROM rt_twin WHERE score > 120 AND score < 135", &[])
        .unwrap();
    let text = rows_exact(&plan).join("\n");
    assert!(
        !text.contains("Index Range Scan"),
        "twin EXPLAIN must not show a range scan, got:\n{text}"
    );

    // A range matching >25% of the table is handed to the sequential scan by
    // the selectivity guard — EXPLAIN must not advertise an index range scan
    // the executor would not use (displayed plan == executed plan).
    let plan = db
        .query("EXPLAIN SELECT id FROM rt_indexed WHERE score > 10 AND score < 90", &[])
        .unwrap();
    let text = rows_exact(&plan).join("\n");
    assert!(
        !text.contains("Index Range Scan"),
        "non-selective range must not be advertised as an index range scan, got:\n{text}"
    );
}

#[test]
fn explain_shows_ordered_index_topk() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    // ORDER BY indexed_col ASC LIMIT k → the Sort node is replaced by the
    // ordered index iteration and EXPLAIN must say so.
    let plan = db
        .query("EXPLAIN SELECT id, score FROM rt_indexed ORDER BY score LIMIT 5", &[])
        .unwrap();
    let text = rows_exact(&plan).join("\n");
    assert!(
        text.contains("Index Ordered Scan using idx_rt_score"),
        "EXPLAIN must surface the ordered index top-k, got:\n{text}"
    );

    // DESC is not served by the fast path — EXPLAIN must keep the Sort.
    let plan = db
        .query(
            "EXPLAIN SELECT id, score FROM rt_indexed ORDER BY score DESC LIMIT 5",
            &[],
        )
        .unwrap();
    let text = rows_exact(&plan).join("\n");
    assert!(
        !text.contains("Index Ordered Scan"),
        "DESC must not be advertised as ordered index iteration, got:\n{text}"
    );

    // The twin (no index) must not claim it either.
    let plan = db
        .query("EXPLAIN SELECT id, score FROM rt_twin ORDER BY score LIMIT 5", &[])
        .unwrap();
    let text = rows_exact(&plan).join("\n");
    assert!(
        !text.contains("Index Ordered Scan"),
        "twin EXPLAIN must not show ordered index iteration, got:\n{text}"
    );
}

#[test]
fn high_selectivity_range_falls_back_to_scan_but_stays_correct() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    build_pair(&db);

    // Matches ~all rows: the selectivity guard hands this to the sequential
    // scan path; results must be identical either way.
    assert_twin_match(&db, "score > -10000");
}

#[test]
fn composite_string_keys_with_nul_bytes_stay_distinct() {
    use heliosdb_nano::storage::ArtIndexManager;

    // Encoding v2 escapes 0x00 inside composite string values so
    // ("a\0", "b") and ("a", "\0b") can no longer collide (latent v1 bug).
    let left = ArtIndexManager::encode_key(&[Value::String("a\0".into()), Value::String("b".into())]);
    let right = ArtIndexManager::encode_key(&[Value::String("a".into()), Value::String("\0b".into())]);
    assert_ne!(left, right);

    // Single-column keys stay raw (range scans rely on byte order).
    let single = ArtIndexManager::encode_key(&[Value::String("a\0b".into())]);
    assert_eq!(single, b"a\0b".to_vec());
}

#[test]
fn sign_flip_encoding_orders_negative_integers() {
    use heliosdb_nano::storage::ArtIndexManager;

    let mut keys: Vec<(i64, Vec<u8>)> = [-100i64, -1, 0, 1, 100, i64::MIN, i64::MAX]
        .into_iter()
        .map(|v| (v, ArtIndexManager::encode_key(&[Value::Int8(v)])))
        .collect();
    keys.sort_by(|a, b| a.1.cmp(&b.1));
    let order: Vec<i64> = keys.into_iter().map(|(v, _)| v).collect();
    assert_eq!(order, vec![i64::MIN, -100, -1, 0, 1, 100, i64::MAX]);
}

// ── IN-list index multi-probe (next-batch item 1) ──────────────────────────
// `col IN (…)` on an indexed column probes the index per value (unioned) rather
// than seq-scanning. Correctness vs an unindexed row twin; EXPLAIN must show the
// probe (matching what actually executes); literal and parameterized forms agree.

#[test]
fn in_list_index_probe_matches_twin_and_explains() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for t in ["ii_indexed", "ii_twin"] {
        db.execute(&format!("CREATE TABLE {t} (aid INT, v INT)")).unwrap();
    }
    db.execute("CREATE INDEX ii_idx ON ii_indexed(aid)").unwrap();
    for t in ["ii_indexed", "ii_twin"] {
        let vals: String = (1..=500).map(|i| format!("({i},{})", (i * 7) % 1000)).collect::<Vec<_>>().join(",");
        db.execute(&format!("INSERT INTO {t} VALUES {vals}")).unwrap();
    }

    for q in [
        "SELECT aid, v FROM {T} WHERE aid IN (3, 7, 11)",
        "SELECT aid FROM {T} WHERE aid IN (7, 3, 7, 11, 3)", // dups
        "SELECT aid FROM {T} WHERE aid IN (99998, 99999)",   // no match
        "SELECT aid FROM {T} WHERE aid IN (3, 7, 11) AND v > 0", // IN + other
        "SELECT aid FROM {T} WHERE aid IN (3, 7, 11) OR aid = 400", // OR (falls back, still correct)
    ] {
        let indexed = db.query(&q.replace("{T}", "ii_indexed"), &[]).unwrap();
        let twin = db.query(&q.replace("{T}", "ii_twin"), &[]).unwrap();
        assert_eq!(rows_sorted(&indexed), rows_sorted(&twin), "divergence for: {q}");
    }

    // EXPLAIN surfaces the probe (displayed plan == executed plan).
    let plan = db.query("EXPLAIN SELECT v FROM ii_indexed WHERE aid IN (3, 7, 11)", &[]).unwrap();
    let text = rows_exact(&plan).join("\n");
    assert!(text.contains("Index IN Probe"), "EXPLAIN must show the IN probe, got:\n{text}");
    // Unindexed twin must NOT claim a probe.
    let plan = db.query("EXPLAIN SELECT v FROM ii_twin WHERE aid IN (3, 7, 11)", &[]).unwrap();
    assert!(!rows_exact(&plan).join("\n").contains("Index IN Probe"));

    // Parameterized IN (A2-normalized shape) agrees with literal.
    let lit = db.query("SELECT aid FROM ii_indexed WHERE aid IN (3,7,11) ORDER BY aid", &[]).unwrap();
    let par = db
        .query_params(
            "SELECT aid FROM ii_indexed WHERE aid IN ($1,$2,$3,$3) ORDER BY aid",
            &[Value::Int4(3), Value::Int4(7), Value::Int4(11)],
        )
        .unwrap();
    assert_eq!(rows_exact(&lit), rows_exact(&par));
}
