//! R2.3 — in-transaction reads must not fall off a cliff.
//!
//! Inside an explicit transaction (the psycopg2/JDBC default!) Nano used to
//! disable ART index point lookups, the kNN fast path, and aggregate/filter
//! pushdowns unconditionally — every SELECT degraded to a full-scan Volcano
//! plan merged with the write set. R2.3 relaxes those gates for the only case
//! where it is provably safe: a **ReadCommitted session transaction with no
//! staged writes touching the target table** (the per-statement snapshot
//! refresh plus the v3.39 snapshot barrier make a current-storage index probe
//! equivalent to the snapshot read).
//!
//! These tests pin the contract from four sides:
//! (a) RC txn with no writes: point lookups return exactly the committed
//!     view another connection sees, and follow RC per-statement freshness;
//! (b) RC txn WITH writes to the table: read-your-writes is NOT regressed
//!     (the slow write-set-merging path is retained);
//! (c) RepeatableRead txns still do NOT see commits made after their
//!     snapshot — they stay on the slow path despite the relaxation;
//! (d) HAVING returns identical results in and out of transactions (it now
//!     runs as a post-filter over the aggregate pushdown output);
//! (e) kNN inside an RC txn with no vector-table writes serves index-path
//!     results identical to autocommit, and in-txn vector DML still follows
//!     the slow read-your-writes path.

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn sorted_debug(rows: &[Tuple]) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().map(|t| format!("{:?}", t.values)).collect();
    out.sort();
    out
}

fn setup_rows(db: &EmbeddedDatabase, n: i32) {
    db.execute("CREATE TABLE t (id INT4 PRIMARY KEY, v TEXT)").unwrap();
    for chunk in (1..=n).collect::<Vec<_>>().chunks(100) {
        let values: Vec<String> = chunk.iter().map(|i| format!("({i}, 'v{i}')")).collect();
        db.execute(&format!("INSERT INTO t (id, v) VALUES {}", values.join(", ")))
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// (a) RC txn with no writes: indexed point lookup == committed view
// ---------------------------------------------------------------------------

#[test]
fn rc_txn_no_writes_point_lookup_matches_committed_view() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_rows(&db, 500);
    db.execute("CREATE INDEX t_v_idx ON t (v)").unwrap();

    // The committed view, as a second (autocommit) connection sees it.
    let expected_pk = db.query("SELECT * FROM t WHERE id = 42", &[]).unwrap();
    let expected_sec = db.query("SELECT * FROM t WHERE v = 'v123'", &[]).unwrap();
    assert_eq!(expected_pk.len(), 1);
    assert_eq!(expected_sec.len(), 1);

    let a = db.create_wire_session("a").unwrap(); // ReadCommitted
    db.execute_for_session(a, "BEGIN").unwrap();

    // PK (ART) point lookup inside the transaction: identical rows.
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE id = 42")
        .unwrap();
    assert_eq!(
        sorted_debug(&rows),
        sorted_debug(&expected_pk),
        "RC txn point lookup must return the committed view"
    );

    // Secondary-index point lookup inside the transaction: identical rows.
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE v = 'v123'")
        .unwrap();
    assert_eq!(
        sorted_debug(&rows),
        sorted_debug(&expected_sec),
        "RC txn secondary-index lookup must return the committed view"
    );

    // ReadCommitted freshness: a commit from another connection mid-txn is
    // visible to the NEXT statement (per-statement snapshot refresh) — and the
    // relaxed index probe must agree with that snapshot.
    db.execute("INSERT INTO t (id, v) VALUES (9999, 'fresh')").unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE id = 9999")
        .unwrap();
    assert_eq!(rows.len(), 1, "RC statement must see the fresh commit");
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("fresh".into()));

    // Filtered COUNT pushdowns agree with the committed view too.
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT COUNT(*) FROM t WHERE id <= 500")
        .unwrap();
    assert_eq!(rows[0].get(0).unwrap(), &Value::Int8(500));

    db.execute_for_session(a, "COMMIT").unwrap();
    db.destroy_session(a).unwrap();
}

#[test]
fn rc_txn_writes_to_other_table_keep_fast_reads_correct() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_rows(&db, 200);
    db.execute("CREATE TABLE other (id INT4 PRIMARY KEY, v TEXT)").unwrap();

    let a = db.create_wire_session("a").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();
    // Stage a write to a DIFFERENT table: reads of `t` keep the fast path,
    // reads of `other` must take the slow path (read-your-writes).
    db.execute_for_session(a, "INSERT INTO other (id, v) VALUES (1, 'staged')")
        .unwrap();

    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE id = 77")
        .unwrap();
    assert_eq!(rows.len(), 1, "point lookup on unwritten table must work");
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("v77".into()));

    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM other WHERE id = 1")
        .unwrap();
    assert_eq!(rows.len(), 1, "read-your-writes on the written table");

    db.execute_for_session(a, "ROLLBACK").unwrap();
    db.destroy_session(a).unwrap();
    let rows = db.query("SELECT * FROM other", &[]).unwrap();
    assert_eq!(rows.len(), 0, "rolled-back staged row must vanish");
}

// ---------------------------------------------------------------------------
// (b) RC txn that wrote the table: read-your-writes NOT regressed
// ---------------------------------------------------------------------------

#[test]
fn rc_txn_with_writes_keeps_read_your_writes() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_rows(&db, 100);

    let a = db.create_wire_session("a").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();

    // Staged INSERT must be visible to its own point lookup.
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1001, 'mine')")
        .unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE id = 1001")
        .unwrap();
    assert_eq!(rows.len(), 1, "txn must see its own staged INSERT");
    assert_eq!(rows[0].get(1).unwrap(), &Value::String("mine".into()));

    // Staged UPDATE must be visible.
    db.execute_for_session(a, "UPDATE t SET v = 'updated' WHERE id = 5")
        .unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT v FROM t WHERE id = 5")
        .unwrap();
    assert_eq!(rows[0].get(0).unwrap(), &Value::String("updated".into()));

    // Staged DELETE must be invisible.
    db.execute_for_session(a, "DELETE FROM t WHERE id = 7").unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE id = 7")
        .unwrap();
    assert_eq!(rows.len(), 0, "txn must not see its own staged DELETE");

    // Filtered aggregate over the written table merges the write set.
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT COUNT(*) FROM t WHERE id > 1000")
        .unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &Value::Int8(1),
        "aggregate must include staged row"
    );

    // ROLLBACK undoes everything; autocommit view is unchanged.
    db.execute_for_session(a, "ROLLBACK").unwrap();
    db.destroy_session(a).unwrap();

    assert_eq!(db.query("SELECT * FROM t WHERE id = 1001", &[]).unwrap().len(), 0);
    let rows = db.query("SELECT v FROM t WHERE id = 5", &[]).unwrap();
    assert_eq!(rows[0].get(0).unwrap(), &Value::String("v5".into()));
    assert_eq!(db.query("SELECT * FROM t WHERE id = 7", &[]).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// (c) RepeatableRead: later commits stay invisible (slow path retained)
// ---------------------------------------------------------------------------

#[test]
fn rr_txn_does_not_see_later_commits() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_rows(&db, 50);

    let s = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(s).unwrap();

    // Anchor the snapshot with a first read.
    let (rows, _) = db
        .query_with_columns_for_session(s, "SELECT COUNT(*) FROM t WHERE id <= 50")
        .unwrap();
    assert_eq!(rows[0].get(0).unwrap(), &Value::Int8(50));
    let (rows, _) = db
        .query_with_columns_for_session(s, "SELECT * FROM t WHERE id = 999")
        .unwrap();
    assert_eq!(rows.len(), 0);

    // Concurrent autocommit writes AFTER the snapshot.
    db.execute("INSERT INTO t (id, v) VALUES (999, 'post-snapshot')")
        .unwrap();
    db.execute("UPDATE t SET v = 'changed' WHERE id = 1").unwrap();

    // The RR transaction must not see any of it — point lookup…
    let (rows, _) = db
        .query_with_columns_for_session(s, "SELECT * FROM t WHERE id = 999")
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "RR txn saw a post-snapshot INSERT via a relaxed index probe"
    );

    // …updated row…
    let (rows, _) = db
        .query_with_columns_for_session(s, "SELECT v FROM t WHERE id = 1")
        .unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &Value::String("v1".into()),
        "RR txn saw a post-snapshot UPDATE"
    );

    // …or filtered aggregates.
    let (rows, _) = db
        .query_with_columns_for_session(s, "SELECT COUNT(*) FROM t WHERE id <= 1000")
        .unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &Value::Int8(50),
        "RR txn aggregate leaked a post-snapshot commit"
    );

    db.rollback_transaction_for_session(s).unwrap();
    db.destroy_session(s).unwrap();

    // Outside the transaction everything is visible.
    assert_eq!(db.query("SELECT * FROM t WHERE id = 999", &[]).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// (d) HAVING: identical results in and out of transactions
// ---------------------------------------------------------------------------

#[test]
fn having_results_identical_in_and_out_of_txn() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE sales (id INT4 PRIMARY KEY, dept TEXT, amount INT4)")?;
    db.execute(
        "INSERT INTO sales (id, dept, amount) VALUES \
         (1, 'a', 10), (2, 'a', 20), (3, 'b', 100), (4, 'b', 5), \
         (5, 'c', 1), (6, 'c', 2), (7, 'd', 500)",
    )?;

    let queries = [
        // GROUP BY + HAVING over an aggregate (post-filter drops groups a, c)
        "SELECT dept, SUM(amount) FROM sales GROUP BY dept HAVING SUM(amount) > 30",
        // HAVING over a different aggregate than the projected one
        "SELECT dept, COUNT(id) FROM sales GROUP BY dept HAVING SUM(amount) > 30",
        // no GROUP BY: HAVING filters the single global row
        "SELECT SUM(amount) FROM sales HAVING SUM(amount) > 1000",
        "SELECT SUM(amount) FROM sales HAVING SUM(amount) > 100",
        // pushdown-eligible predicate + HAVING combined
        "SELECT dept, SUM(amount) FROM sales WHERE amount >= 5 GROUP BY dept HAVING COUNT(id) >= 2",
    ];

    for sql in queries {
        let autocommit = db.query(sql, &[])?;

        // RC session txn, no writes → pushdown + HAVING post-filter.
        let a = db.create_wire_session("a")?;
        db.execute_for_session(a, "BEGIN")?;
        let (no_writes, _) = db.query_with_columns_for_session(a, sql)?;
        // Same txn, after writing the table → slow path, same answer for the
        // unchanged data (write doesn't satisfy any HAVING group's predicate
        // differently: id 999 dept 'zzz' amount 0 forms no qualifying group).
        db.execute_for_session(a, "INSERT INTO sales (id, dept, amount) VALUES (999, 'zzz', 0)")?;
        let (with_writes, _) = db.query_with_columns_for_session(a, sql)?;
        db.execute_for_session(a, "ROLLBACK")?;
        db.destroy_session(a)?;

        assert_eq!(
            sorted_debug(&no_writes),
            sorted_debug(&autocommit),
            "HAVING diverged in RC txn (no writes) for: {sql}"
        );
        assert_eq!(
            sorted_debug(&with_writes),
            sorted_debug(&autocommit),
            "HAVING diverged in RC txn (with writes) for: {sql}"
        );
    }

    // Literal expectation for the headline case so a both-sides-wrong parity
    // bug cannot slip through.
    let rows = db.query(
        "SELECT dept, SUM(amount) FROM sales GROUP BY dept HAVING SUM(amount) > 30",
        &[],
    )?;
    let mut groups: Vec<(String, i64)> = rows
        .iter()
        .map(|t| {
            let dept = match t.get(0).unwrap() {
                Value::String(s) => s.clone(),
                other => panic!("expected text dept, got {other:?}"),
            };
            let sum = match t.get(1).unwrap() {
                Value::Int8(v) => *v,
                Value::Int4(v) => i64::from(*v),
                other => panic!("expected int sum, got {other:?}"),
            };
            (dept, sum)
        })
        .collect();
    groups.sort();
    assert_eq!(groups, vec![("b".to_string(), 105), ("d".to_string(), 500)]);
    Ok(())
}

// ---------------------------------------------------------------------------
// (e) kNN inside RC txn
// ---------------------------------------------------------------------------

#[test]
fn knn_inside_rc_txn_no_writes_serves_index_path_results() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE vecs (id INT4, embedding VECTOR(3))")?;
    db.execute("INSERT INTO vecs VALUES (1, '[5.0, 0.0, 0.0]')")?;
    db.execute("INSERT INTO vecs VALUES (2, '[0.0, 5.0, 0.0]')")?;
    db.execute("INSERT INTO vecs VALUES (3, '[0.2, 0.0, 0.0]')")?;
    db.execute("CREATE INDEX vecs_idx ON vecs USING hnsw (embedding vector_l2_ops)")?;

    let knn = "SELECT id FROM vecs ORDER BY embedding <-> '[0.0, 0.0, 0.0]' LIMIT 1";
    let autocommit = db.query(knn, &[])?;
    assert_eq!(autocommit.len(), 1);
    assert_eq!(autocommit[0].get(0).unwrap(), &Value::Int4(3));

    let a = db.create_wire_session("a")?;
    db.execute_for_session(a, "BEGIN")?;

    // No writes to vecs: the HNSW index path answers, identical to autocommit.
    let (rows, _) = db.query_with_columns_for_session(a, knn)?;
    assert_eq!(
        sorted_debug(&rows),
        sorted_debug(&autocommit),
        "in-txn kNN != autocommit kNN"
    );

    // RC freshness: a concurrent committed insert that is nearer must be
    // visible to the next statement.
    db.execute("INSERT INTO vecs VALUES (99, '[0.1, 0.0, 0.0]')")?;
    let (rows, _) = db.query_with_columns_for_session(a, knn)?;
    assert_eq!(
        rows[0].get(0).unwrap(),
        &Value::Int4(99),
        "RC kNN must see the fresh commit"
    );

    // Once the txn writes the vector table, reads take the slow path and must
    // see the staged (even nearer) row — read-your-writes.
    db.execute_for_session(a, "INSERT INTO vecs VALUES (100, '[0.01, 0.0, 0.0]')")?;
    let (rows, _) = db.query_with_columns_for_session(a, knn)?;
    assert_eq!(
        rows[0].get(0).unwrap(),
        &Value::Int4(100),
        "kNN must see the txn's own staged vector row"
    );

    db.execute_for_session(a, "ROLLBACK")?;
    db.destroy_session(a)?;

    // After rollback the staged vector is gone everywhere.
    let rows = db.query(knn, &[])?;
    assert_eq!(rows[0].get(0).unwrap(), &Value::Int4(99));
    Ok(())
}

// ---------------------------------------------------------------------------
// ORDER BY ... LIMIT (storage direct top-k) inside RC txn
// ---------------------------------------------------------------------------

#[test]
fn rc_txn_topk_matches_committed_view_and_respects_writes() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    setup_rows(&db, 300);

    let topk = "SELECT id FROM t ORDER BY id DESC LIMIT 3";
    let autocommit = db.query(topk, &[]).unwrap();

    let a = db.create_wire_session("a").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();
    let (rows, _) = db.query_with_columns_for_session(a, topk).unwrap();
    assert_eq!(
        sorted_debug(&rows),
        sorted_debug(&autocommit),
        "in-txn top-k != autocommit top-k"
    );

    // Stage a row beyond the current max: the slow path must surface it.
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (5000, 'top')")
        .unwrap();
    let (rows, _) = db.query_with_columns_for_session(a, topk).unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &Value::Int4(5000),
        "top-k must see the txn's own staged row"
    );
    db.execute_for_session(a, "ROLLBACK").unwrap();
    db.destroy_session(a).unwrap();
}

// ---------------------------------------------------------------------------
// A/B harness (perf attribution, not a correctness test)
//
// Run explicitly with output shown:
//   HELIOS_TXN_READ=1 cargo test --release --test in_txn_read_tests \
//     run_in_txn_read_ab -- --nocapture --test-threads=1
//
// Three configurations over the same 100k-row table:
//   autocommit       — reference (always had the ART probe)
//   txn / no writes  — R2.3 relaxed gate (ART probe inside an RC session txn)
//   txn / with write — slow path retained (== pre-R2.3 behaviour for ALL
//                      in-txn reads): full-scan Volcano + write-set merge
// ---------------------------------------------------------------------------

#[test]
fn run_in_txn_read_ab() {
    if std::env::var("HELIOS_TXN_READ").is_err() {
        eprintln!("skipping run_in_txn_read_ab (set HELIOS_TXN_READ=1)");
        return;
    }
    const ROWS: i32 = 100_000;
    const LOOKUPS: usize = 200;

    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE big (id INT4 PRIMARY KEY, v TEXT)").unwrap();
    for chunk in (1..=ROWS).collect::<Vec<_>>().chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|i| format!("({i}, 'v{i}')")).collect();
        db.execute(&format!("INSERT INTO big (id, v) VALUES {}", values.join(", ")))
            .unwrap();
    }

    let time_lookups = |run: &dyn Fn(&str) -> Vec<Tuple>| -> std::time::Duration {
        // Spread probes across the keyspace; warm up once.
        let _ = run("SELECT * FROM big WHERE id = 1");
        let start = std::time::Instant::now();
        for i in 0..LOOKUPS {
            let id = 1 + ((i as i32) * 499) % ROWS;
            let rows = run(&format!("SELECT * FROM big WHERE id = {id}"));
            assert_eq!(rows.len(), 1, "lookup id {id} must return exactly one row");
        }
        start.elapsed()
    };

    // (1) autocommit reference
    let auto = time_lookups(&|sql| db.query(sql, &[]).unwrap());

    // (2) RC session txn, no writes to `big` (R2.3 fast path)
    let a = db.create_wire_session("ab_nowrites").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();
    let no_writes = time_lookups(&|sql| db.query_with_columns_for_session(a, sql).unwrap().0);
    db.execute_for_session(a, "ROLLBACK").unwrap();
    db.destroy_session(a).unwrap();

    // (3) RC session txn AFTER one staged write to `big` (slow path — this is
    //     what EVERY in-txn read paid before R2.3)
    let b = db.create_wire_session("ab_written").unwrap();
    db.execute_for_session(b, "BEGIN").unwrap();
    db.execute_for_session(b, "INSERT INTO big (id, v) VALUES (1000001, 'staged')")
        .unwrap();
    let with_write = time_lookups(&|sql| db.query_with_columns_for_session(b, sql).unwrap().0);
    db.execute_for_session(b, "ROLLBACK").unwrap();
    db.destroy_session(b).unwrap();

    let per = |d: std::time::Duration| d.as_micros() as f64 / LOOKUPS as f64;
    println!("R2.3 A/B: point lookup on {ROWS}-row table, {LOOKUPS} lookups each");
    println!("  autocommit (reference fast path) : {:>10.1} us/lookup", per(auto));
    println!(
        "  RC txn, no table writes (R2.3)   : {:>10.1} us/lookup",
        per(no_writes)
    );
    println!(
        "  RC txn, staged write (pre-R2.3)  : {:>10.1} us/lookup",
        per(with_write)
    );
    println!(
        "  speedup (slow path / R2.3 path)  : {:>10.1}x",
        per(with_write) / per(no_writes)
    );
}
