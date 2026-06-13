//! R3.5 executor-hygiene timing probes (perf investigation, not correctness).
//!
//! Run explicitly, single-threaded, with output shown:
//!   HELIOS_R35=1 cargo test --profile perf --test r35_probe -- --nocapture --test-threads=1
//!
//! Each probe times one of the R3.5 items' hot paths with `Instant` so the
//! same binary run before/after a commit gives the per-item delta.

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::time::Instant;

fn gated() -> bool {
    if std::env::var("HELIOS_R35").is_err() {
        eprintln!("skipping r35 probe (set HELIOS_R35=1)");
        return false;
    }
    true
}

/// Wide-ish table so the per-row linear schema scan has something to chew on.
fn setup_rows(db: &EmbeddedDatabase, n: usize) {
    db.execute("CREATE TABLE probe (a INTEGER, b INTEGER, c INTEGER, d TEXT, e TEXT, f TEXT, g INTEGER, h INTEGER)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    for i in 0..n {
        db.execute(&format!(
            "INSERT INTO probe (a, b, c, d, e, f, g, h) VALUES ({}, {}, {}, 'd{}', 'e{}', 'f{}', {}, {})",
            i,
            i % 1000,
            (i * 7) % 10000,
            i % 100,
            i % 50,
            i % 25,
            i % 3,
            i % 17
        ))
        .unwrap();
    }
    db.execute("COMMIT").unwrap();
}

fn time_query(db: &EmbeddedDatabase, label: &str, sql: &str, iters: usize) {
    // Warmup. NOTE: every execution gets a unique trailing comment so the
    // consecutive-repeat result cache can never serve a memoized answer —
    // we want the executor hot path, not the cache.
    let warm = db.query(&format!("{sql} -- warm"), &[]).unwrap();
    let mut total_rows = warm.len();
    let start = Instant::now();
    for i in 0..iters {
        total_rows += db.query(&format!("{sql} -- i{i}"), &[]).unwrap().len();
    }
    let elapsed = start.elapsed();
    println!(
        "{label:<42} {iters} iters  {:>9.2} ms total  {:>8.3} ms/iter  (rows/iter={})",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e3 / iters as f64,
        total_rows / (iters + 1)
    );
}

/// Item 1: bound column refs — expression-heavy filter + projection
/// (shapes that dodge the DirectFilterPredicate / direct-index fast paths).
#[test]
fn probe_item1_column_binding() {
    if !gated() {
        return;
    }
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let n = 200_000;
    setup_rows(&db, n);

    println!("\n=== R3.5 item 1 probe: column-ref binding (N={n}) ===");
    time_query(
        &db,
        "filter 3-col arith predicate",
        "SELECT g FROM probe WHERE (a + b) > 100000 AND (c - g) > 5000 AND (h + g) <> 7",
        10,
    );
    time_query(
        &db,
        "projection mixed exprs",
        "SELECT a + b, c * 2, h - g, upper(d) FROM probe WHERE g = 1",
        10,
    );
    time_query(
        &db,
        "order by expression (Sort path)",
        "SELECT a FROM probe WHERE g = 1 ORDER BY (b * 100 - c) + h",
        5,
    );
    println!();
}

/// Item 2: SortOperator decorate-sort-undecorate. ORDER BY without LIMIT so
/// the full SortOperator (not TopK) runs; expression key forces evaluation.
#[test]
fn probe_item2_sort_keys() {
    if !gated() {
        return;
    }
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let n = 200_000;
    setup_rows(&db, n);

    println!("\n=== R3.5 item 2 probe: SortOperator key precompute (N={n}) ===");
    time_query(
        &db,
        "ORDER BY expr, full sort",
        "SELECT a FROM probe ORDER BY (b * 100 - c) + h",
        3,
    );
    time_query(
        &db,
        "ORDER BY two columns, full sort",
        "SELECT a FROM probe ORDER BY b, c DESC",
        3,
    );
    println!();
}

/// Item 3: per-row scalar-function name resolution.
#[test]
fn probe_item3_function_dispatch() {
    if !gated() {
        return;
    }
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let n = 200_000;
    setup_rows(&db, n);

    println!("\n=== R3.5 item 3 probe: scalar function dispatch (N={n}) ===");
    time_query(
        &db,
        "3 scalar funcs per row",
        "SELECT upper(d), lower(e), length(f) FROM probe",
        5,
    );
    time_query(
        &db,
        "scalar func in predicate",
        "SELECT g FROM probe WHERE length(d) + length(e) > 4",
        5,
    );
    println!();
}

/// Item 4: NestedLoopJoin probe clones. A non-equi join forces NLJ
/// (hash join only handles equi-joins).
#[test]
fn probe_item4_nlj() {
    if !gated() {
        return;
    }
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE l (id INTEGER, lo INTEGER, hi INTEGER, tag TEXT, pad1 TEXT, pad2 TEXT)")
        .unwrap();
    db.execute("CREATE TABLE r (id INTEGER, v INTEGER, name TEXT, pad1 TEXT, pad2 TEXT)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    let nl = 2_000;
    let nr = 2_000;
    for i in 0..nl {
        db.execute(&format!(
            "INSERT INTO l (id, lo, hi, tag, pad1, pad2) VALUES ({i}, {}, {}, 't{}', 'padding-one-{i}', 'padding-two-{i}')",
            (i * 13) % 100000,
            (i * 13) % 100000 + 40,
            i % 7
        ))
        .unwrap();
    }
    for i in 0..nr {
        db.execute(&format!(
            "INSERT INTO r (id, v, name, pad1, pad2) VALUES ({i}, {}, 'n{}', 'r-padding-one-{i}', 'r-padding-two-{i}')",
            (i * 29) % 100000,
            i % 11
        ))
        .unwrap();
    }
    db.execute("COMMIT").unwrap();

    println!("\n=== R3.5 item 4 probe: NLJ probe clones ({nl}x{nr} pairs) ===");
    time_query(
        &db,
        "range (non-equi) inner join",
        "SELECT count(*) FROM l JOIN r ON r.v >= l.lo AND r.v < l.hi",
        3,
    );
    time_query(
        &db,
        "non-equi left join",
        "SELECT count(*) FROM l LEFT JOIN r ON r.v > l.lo AND r.v < l.hi AND l.id <> r.id",
        3,
    );
    println!();
}

/// Item 5: CTE deep-clone per reference. One materialized CTE referenced
/// several times; rows are wide-ish strings so the clone cost is visible.
#[test]
fn probe_item5_cte_clone() {
    if !gated() {
        return;
    }
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let n = 100_000;
    setup_rows(&db, n);

    println!("\n=== R3.5 item 5 probe: CTE serve cost (N={n}) ===");
    time_query(
        &db,
        "CTE referenced 3x",
        "WITH big AS (SELECT a, b, c, d, e, f FROM probe) \
         SELECT (SELECT count(*) FROM big) + (SELECT count(*) FROM big) + (SELECT count(*) FROM big)",
        3,
    );
    time_query(
        &db,
        "CTE + LIMIT (partial consume)",
        "WITH big AS (SELECT a, b, c, d, e, f FROM probe) SELECT a FROM big LIMIT 10",
        20,
    );
    println!();
}

// Silence unused-import warning when the gate is off at compile time.
#[allow(dead_code)]
fn _unused(_v: Value) {}
