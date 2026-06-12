//! R4.2 / R4.4 performance probes (foreground, release profile).
//!
//! Run explicitly:
//! ```sh
//! cargo test --release --test r4_index_probes -- --ignored --nocapture --test-threads=1
//! ```
//! Sizes are env-tunable: `HELIOS_PROBE_ROWS` (default 1_000_000),
//! `HELIOS_PROBE_VECTORS` (default 50_000), `HELIOS_PROBE_VEC_DIM` (default 64).

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::time::Instant;
use tempfile::TempDir;

fn probe_rows() -> usize {
    std::env::var("HELIOS_PROBE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000)
}

fn probe_vectors() -> usize {
    std::env::var("HELIOS_PROBE_VECTORS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000)
}

fn probe_vec_dim() -> usize {
    std::env::var("HELIOS_PROBE_VEC_DIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

/// Bulk-insert `n` rows: id PK, score uniform in [0, 1000), name text.
fn fill_rows(db: &EmbeddedDatabase, table: &str, n: usize) {
    let batch = 1_000;
    let mut i = 0;
    while i < n {
        let mut sql = format!("INSERT INTO {table} VALUES ");
        let end = (i + batch).min(n);
        for row in i..end {
            if row > i {
                sql.push(',');
            }
            let score = (row.wrapping_mul(2654435761)) % 1000;
            sql.push_str(&format!("({row}, {score}, 'name{:04}')", row % 5000));
        }
        db.execute(&sql).unwrap();
        i = end;
    }
}

fn fill_vectors(db: &EmbeddedDatabase, table: &str, n: usize, dim: usize) {
    let batch = 500;
    let mut i = 0;
    while i < n {
        let mut sql = format!("INSERT INTO {table} VALUES ");
        let end = (i + batch).min(n);
        for row in i..end {
            if row > i {
                sql.push(',');
            }
            let mut vec_lit = String::with_capacity(dim * 8);
            vec_lit.push('[');
            for d in 0..dim {
                if d > 0 {
                    vec_lit.push(',');
                }
                let v = ((row.wrapping_mul(31).wrapping_add(d * 7)) % 1000) as f32 / 1000.0;
                vec_lit.push_str(&format!("{v:.3}"));
            }
            vec_lit.push(']');
            sql.push_str(&format!("({row}, '{vec_lit}')"));
        }
        db.execute(&sql).unwrap();
        i = end;
    }
}

/// Monotonic counter so every issued statement is textually unique: the
/// result cache is keyed on the exact SQL string, and repeated identical
/// SELECTs would otherwise measure LRU result-cache hits, not execution.
static QUERY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn uncached(sql: &str) -> String {
    let n = QUERY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{sql} /* probe {n} */")
}

fn time_query(db: &EmbeddedDatabase, sql: &str, reps: usize) -> (f64, usize) {
    // Warm once, then take the best of `reps` (quiet-machine convention).
    let rows = db.query(&uncached(sql), &[]).unwrap().len();
    let mut best = f64::MAX;
    for _ in 0..reps {
        let stmt = uncached(sql);
        let started = Instant::now();
        let got = db.query(&stmt, &[]).unwrap().len();
        assert_eq!(got, rows);
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    (best, rows)
}

/// R4.2 probe: open time with snapshots (clean restart) vs rebuild (crash).
#[test]
#[ignore = "foreground probe — run with --release --ignored --nocapture"]
fn probe_open_time_snapshot_vs_rebuild() {
    let rows = probe_rows();
    let vectors = probe_vectors();
    let dim = probe_vec_dim();
    let temp = TempDir::new().unwrap();

    {
        let db = EmbeddedDatabase::new(temp.path()).unwrap();
        db.execute("CREATE TABLE p_rows (id INT PRIMARY KEY, score INT, name TEXT)")
            .unwrap();
        db.execute("CREATE INDEX idx_p_rows_score ON p_rows(score)").unwrap();
        let started = Instant::now();
        fill_rows(&db, "p_rows", rows);
        println!("load: {} rows in {:.1}s", rows, started.elapsed().as_secs_f64());

        db.execute(&format!("CREATE TABLE p_vecs (id INT PRIMARY KEY, emb VECTOR({dim}))"))
            .unwrap();
        let started = Instant::now();
        fill_vectors(&db, "p_vecs", vectors, dim);
        println!("load: {} vectors in {:.1}s", vectors, started.elapsed().as_secs_f64());
        let started = Instant::now();
        db.execute("CREATE INDEX idx_p_vecs_emb ON p_vecs USING hnsw (emb)").unwrap();
        println!("hnsw build: {:.1}s", started.elapsed().as_secs_f64());

        // Simulate crash → next open must rebuild everything (the "before").
        db.storage.set_index_snapshots_on_close(false);
    }

    let started = Instant::now();
    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let rebuild_open_s = started.elapsed().as_secs_f64();
    let report = db.storage.last_index_open_report().unwrap();
    assert_eq!(report.tables_from_snapshot, 0, "{report:?}");
    assert_eq!(report.vector_reloaded, 0, "{report:?}");
    println!(
        "open (REBUILD path, pre-R4.2 behaviour): {:.2}s  [{} rows scanned, {} vector rebuilt]",
        rebuild_open_s, report.rows_scanned, report.vector_rebuilt
    );
    // Clean close → snapshots written (the "after").
    let started = Instant::now();
    drop(db);
    println!("checkpoint at close: {:.2}s", started.elapsed().as_secs_f64());

    let started = Instant::now();
    let db = EmbeddedDatabase::new(temp.path()).unwrap();
    let snapshot_open_s = started.elapsed().as_secs_f64();
    let report = db.storage.last_index_open_report().unwrap();
    assert!(report.tables_from_snapshot >= 1, "{report:?}");
    assert_eq!(report.rows_scanned, 0, "{report:?}");
    assert_eq!(report.vector_reloaded, 1, "{report:?}");
    println!(
        "open (SNAPSHOT path, R4.2): {:.2}s  [{} entries loaded, {} vector reloaded]",
        snapshot_open_s, report.entries_loaded, report.vector_reloaded
    );
    println!(
        "open speedup: {:.1}x  ({:.2}s -> {:.2}s)",
        rebuild_open_s / snapshot_open_s.max(1e-9),
        rebuild_open_s,
        snapshot_open_s
    );

    // Sanity: data still queryable.
    let n = db.query("SELECT COUNT(*) FROM p_rows", &[]).unwrap();
    assert_eq!(n[0].values.first(), Some(&Value::Int8(rows as i64)));
}

/// R4.4 probe: range queries at 0.1% / 1% / 10% selectivity, indexed fast
/// path vs the same engine with the fast path disabled (full scan + filter).
#[test]
#[ignore = "foreground probe — run with --release --ignored --nocapture"]
fn probe_range_scan_selectivity() {
    let rows = probe_rows();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p_range (id INT PRIMARY KEY, score INT, name TEXT)")
        .unwrap();
    fill_rows(&db, "p_range", rows);
    db.execute("CREATE INDEX idx_p_range_score ON p_range(score)").unwrap();

    // score is uniform over [0, 1000): a span of S values ≈ S/1000 selectivity.
    let cases = [("0.1%", "score >= 100 AND score < 101"),
                 ("1%",   "score >= 100 AND score < 110"),
                 ("10%",  "score >= 100 AND score < 200")];

    println!("\nrange scan @ {} rows (ms, best of 5):", rows);
    println!("{:<6} {:>12} {:>12} {:>9} {:>10}", "sel", "indexed", "full-scan", "speedup", "rows");
    for (label, predicate) in cases {
        let sql = format!("SELECT id FROM p_range WHERE {predicate}");
        let (indexed_ms, n1) = time_query(&db, &sql, 5);
        std::env::set_var("HELIOS_INDEX_RANGE_OFF", "1");
        let (scan_ms, n2) = time_query(&db, &sql, 5);
        std::env::remove_var("HELIOS_INDEX_RANGE_OFF");
        assert_eq!(n1, n2, "fast path row count mismatch at {label}");
        println!(
            "{:<6} {:>10.2}ms {:>10.2}ms {:>8.1}x {:>10}",
            label,
            indexed_ms,
            scan_ms,
            scan_ms / indexed_ms.max(1e-9),
            n1
        );
    }
}

/// R4.4 probe: ORDER BY indexed_col LIMIT k via ordered index iteration.
#[test]
#[ignore = "foreground probe — run with --release --ignored --nocapture"]
fn probe_order_by_limit() {
    let rows = probe_rows();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p_topk (id INT PRIMARY KEY, score INT, name TEXT)")
        .unwrap();
    fill_rows(&db, "p_topk", rows);
    db.execute("CREATE INDEX idx_p_topk_score ON p_topk(score)").unwrap();

    println!("\nORDER BY score LIMIT k @ {} rows (ms, best of 5):", rows);
    println!("{:<22} {:>12} {:>12} {:>9}", "query", "indexed", "generic", "speedup");
    for k in [1usize, 10, 100] {
        let sql = format!("SELECT id, score FROM p_topk ORDER BY score LIMIT {k}");
        let (indexed_ms, _) = time_query(&db, &sql, 5);
        std::env::set_var("HELIOS_INDEX_RANGE_OFF", "1");
        let (generic_ms, _) = time_query(&db, &sql, 5);
        std::env::remove_var("HELIOS_INDEX_RANGE_OFF");
        println!(
            "{:<22} {:>10.2}ms {:>10.2}ms {:>8.1}x",
            format!("ORDER BY ... LIMIT {k}"),
            indexed_ms,
            generic_ms,
            generic_ms / indexed_ms.max(1e-9)
        );
    }
}

/// Diagnostic: cold-row range cost. The row cache holds 10k entries, so
/// disjoint 1% slices (10k rows each at 1M) are always fetched cold — this
/// measures the true first-touch cost of the index path (k point gets)
/// against the sequential scan, before and after RocksDB compaction, and is
/// what the selectivity guard must be judged against.
#[test]
#[ignore = "foreground probe — run with --release --ignored --nocapture"]
fn probe_cold_range_cost_and_compaction() {
    let rows = probe_rows();
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p_cold (id INT PRIMARY KEY, score INT, name TEXT)")
        .unwrap();
    fill_rows(&db, "p_cold", rows);
    db.execute("CREATE INDEX idx_p_cold_score ON p_cold(score)").unwrap();

    let slice_sql =
        |r: usize| format!("SELECT id FROM p_cold WHERE score >= {} AND score < {}", 10 * r, 10 * r + 10);
    let run_one = |sql: &str| -> (f64, usize) {
        let stmt = uncached(sql);
        let started = Instant::now();
        let n = db.query(&stmt, &[]).unwrap().len();
        (started.elapsed().as_secs_f64() * 1000.0, n)
    };

    println!("\ncold 1% slices @ {} rows (each slice touched once):", rows);
    for r in 0..3 {
        let (ms, n) = run_one(&slice_sql(r));
        println!("  uncompacted indexed slice {r}: {ms:>10.0}ms ({n} rows)");
    }
    std::env::set_var("HELIOS_INDEX_RANGE_OFF", "1");
    let (ms, n) = run_one(&slice_sql(3));
    std::env::remove_var("HELIOS_INDEX_RANGE_OFF");
    println!("  uncompacted full-scan slice 3: {ms:>9.0}ms ({n} rows)");

    let started = Instant::now();
    db.storage.vacuum().unwrap();
    println!("  vacuum/compaction: {:.1}s", started.elapsed().as_secs_f64());

    for r in 4..7 {
        let (ms, n) = run_one(&slice_sql(r));
        println!("  compacted indexed slice {r}:   {ms:>10.0}ms ({n} rows)");
    }
    std::env::set_var("HELIOS_INDEX_RANGE_OFF", "1");
    let (ms, n) = run_one(&slice_sql(7));
    std::env::remove_var("HELIOS_INDEX_RANGE_OFF");
    println!("  compacted full-scan slice 7:   {ms:>9.0}ms ({n} rows)");
}

/// R4 parity probe: point lookups and DML on an indexed table — the R4.2
/// steady-state cost is one relaxed atomic load per index mutation plus a
/// one-time marker delete after each checkpoint, so DML throughput must be
/// unchanged (<5% vs pre-R4 baselines measured in merge validation).
#[test]
#[ignore = "foreground probe — run with --release --ignored --nocapture"]
fn probe_point_lookup_and_dml_parity() {
    let rows = probe_rows().min(200_000);
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p_dml (id INT PRIMARY KEY, score INT, name TEXT)")
        .unwrap();
    db.execute("CREATE INDEX idx_p_dml_score ON p_dml(score)").unwrap();

    let started = Instant::now();
    fill_rows(&db, "p_dml", rows);
    let insert_s = started.elapsed().as_secs_f64();
    println!(
        "\nINSERT (PK + secondary index maintained): {} rows in {:.2}s = {:.0} rows/s",
        rows,
        insert_s,
        rows as f64 / insert_s
    );

    let (ms, n) = time_query(&db, "SELECT id, score FROM p_dml WHERE id = 12345", 20);
    println!("point lookup by PK: {:.3}ms ({} row)", ms, n);
    let (ms, n) = time_query(&db, "SELECT id, score FROM p_dml WHERE score = 123", 20);
    println!("point lookup by secondary index: {:.3}ms ({} rows)", ms, n);

    let started = Instant::now();
    let updates = 10_000.min(rows);
    for i in 0..updates {
        db.execute(&format!("UPDATE p_dml SET score = score + 1 WHERE id = {i}"))
            .unwrap();
    }
    let s = started.elapsed().as_secs_f64();
    println!("UPDATE (index column): {} ops in {:.2}s = {:.0} ops/s", updates, s, updates as f64 / s);

    let started = Instant::now();
    let deletes = 10_000.min(rows);
    for i in 0..deletes {
        db.execute(&format!("DELETE FROM p_dml WHERE id = {i}")).unwrap();
    }
    let s = started.elapsed().as_secs_f64();
    println!("DELETE by PK: {} ops in {:.2}s = {:.0} ops/s", deletes, s, deletes as f64 / s);
}
