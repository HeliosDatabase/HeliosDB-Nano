//! Pagination depth curve — does latency actually stay flat as OFFSET grows?
//!
//! `README.md` and `docs/PERFORMANCE.md` advertise deep `LIMIT … OFFSET` as
//! "constant-time (~30 µs regardless of offset)". Constant-time is a claim about
//! a SLOPE, and no single latency number can support it, so this measures the
//! curve: the same query at increasing depths in one session, reporting the
//! ratio between the deepest and shallowest point.
//!
//! Run (not part of the default suite — it is a measurement, not an assertion):
//!   cargo test --release --test pagination_depth_curve -- --ignored --nocapture
//!
//! Writes `perf/pagination_depth_curve.json` so the published claim has a
//! committed artifact behind it. Row count is pinned to 10_000, the documented
//! full-suite cap for this host; do not raise it without re-reading the resource
//! constraints in CLAUDE.md.

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::time::Instant;

const ROWS: i64 = 10_000;
const PAGE: i64 = 10;
const WARMUP: usize = 3;
const REPEATS: usize = 25;

fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE p (id INT PRIMARY KEY, created_at INT, label TEXT)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    for i in 0..ROWS {
        db.execute(&format!(
            "INSERT INTO p VALUES ({}, {}, 'label_{}')",
            i,
            1_000_000 + i,
            i
        ))
        .unwrap();
    }
    db.execute("COMMIT").unwrap();
}

/// p50 in microseconds over REPEATS runs, each with DISTINCT SQL text.
///
/// Repeating one query text measures the SQL-text-keyed result cache, not
/// pagination: after the warmup runs every repeat is a cache hit, which is how
/// an earlier version of this harness reported a flat 0.6 us for a full-scan
/// tuple filter at depth 9000 (~66 ps/row — impossible). Each repeat therefore
/// nudges the depth by `i`, which both defeats the cache and matches how
/// pagination is actually used: a client walking pages issues a different
/// offset every time, so it never gets a cache hit either. The extra `i` rows
/// of work are negligible against the depth being measured.
fn p50_us(db: &EmbeddedDatabase, make: &dyn Fn(i64) -> String, depth: i64) -> (f64, usize) {
    // Warm the OS/page cache and any schema lookups without touching the
    // texts we are about to measure.
    for i in 0..WARMUP {
        db.query(&make(depth + 500 + i as i64), &[]).unwrap();
    }
    let mut samples = Vec::with_capacity(REPEATS);
    let mut rows = 0usize;
    for i in 0..REPEATS {
        let sql = make(depth + i as i64);
        let t = Instant::now();
        let r = db.query(&sql, &[]).unwrap();
        samples.push(t.elapsed().as_secs_f64() * 1e6);
        rows = r.len();
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (samples[samples.len() / 2], rows)
}

#[test]
#[ignore = "measurement, not an assertion; run with --ignored"]
fn pagination_depth_curve() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);

    let depths = [0i64, 10, 100, 1_000, 5_000, 9_000];
    let mut json_rows: Vec<String> = Vec::new();

    println!("\n=== pagination depth curve (N = {ROWS}, page = {PAGE}) ===");
    println!("{:<34} {:>8} {:>10} {:>7}", "shape / depth", "p50 us", "rows", "vs d=0");

    for (label, make) in [
        (
            "OFFSET, no ORDER BY",
            Box::new(|d: i64| format!("SELECT id FROM p LIMIT {PAGE} OFFSET {d}")) as Box<dyn Fn(i64) -> String>,
        ),
        (
            "OFFSET, ORDER BY id",
            Box::new(|d: i64| format!("SELECT id FROM p ORDER BY id LIMIT {PAGE} OFFSET {d}")),
        ),
        (
            "OFFSET, ORDER BY created_at DESC",
            Box::new(|d: i64| format!("SELECT id FROM p ORDER BY created_at DESC, id DESC LIMIT {PAGE} OFFSET {d}")),
        ),
        (
            "keyset, WHERE id > b",
            Box::new(|d: i64| format!("SELECT id FROM p WHERE id > {d} ORDER BY id LIMIT {PAGE}")),
        ),
        (
            "keyset, tuple (created_at,id)",
            Box::new(|d: i64| {
                format!(
                    "SELECT id FROM p WHERE (created_at, id) > ({}, {}) ORDER BY created_at, id LIMIT {PAGE}",
                    1_000_000 + d,
                    d
                )
            }),
        ),
    ] {
        let mut base = 0.0f64;
        for (i, &d) in depths.iter().enumerate() {
            let (us, rows) = p50_us(&db, &make, d);
            if i == 0 {
                base = us;
            }
            let ratio = if base > 0.0 { us / base } else { f64::NAN };
            println!(
                "{:<34} {:>8.1} {:>10} {:>6.1}x",
                format!("{label} @ {d}"),
                us,
                rows,
                ratio
            );
            json_rows.push(format!(
                "    {{\"shape\": \"{label}\", \"depth\": {d}, \"p50_us\": {us:.2}, \"rows\": {rows}, \"ratio_vs_shallowest\": {ratio:.2}}}"
            ));
        }
        println!();
    }

    let artifact = format!(
        "{{\n  \"rows\": {ROWS},\n  \"page\": {PAGE},\n  \"repeats\": {REPEATS},\n  \"note\": \"p50 over {REPEATS} runs, EACH WITH DISTINCT SQL TEXT (depth+i) so the SQL-text-keyed result cache is bypassed - repeating one text measures cache hits, not pagination. Embedded path, release build. Constant-time is a claim about the slope: read ratio_vs_shallowest.\",\n  \"measurements\": [\n{}\n  ]\n}}\n",
        json_rows.join(",\n")
    );
    std::fs::create_dir_all("perf").ok();
    std::fs::write("perf/pagination_depth_curve.json", &artifact).expect("write artifact");
    println!("wrote perf/pagination_depth_curve.json");
}
