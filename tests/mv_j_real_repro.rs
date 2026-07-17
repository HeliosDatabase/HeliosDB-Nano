//! Quirk J / issue #2 regression — verified against the Token-Dashboard data dir
//! (copied to /tmp/td-j-work, 448,573 rows across multiple SSTs). Guarded on path
//! existence, so it's a no-op in normal CI but verifies the fix when the data dir is
//! present. Covers the full bisection from the issue: COUNT(DISTINCT), COUNT(*)+SUM,
//! GROUP BY, a REFRESH, and persistence across reopen — all of which previously
//! materialized a tiny slice (distinct 4, count 407) for two compounding reasons:
//!   1. the MV materialized under the DDL statement's *implicit transaction* snapshot
//!      (a stale slice) instead of the current branch-aware view; and
//!   2. `store_view_data` layered the new value on top of an *orphaned* `__mv_*` data
//!      row (metadata dropped in a prior run, rows left behind), so `SELECT *` read
//!      the stale row back.
//! The fix materializes via a fresh executor (no active txn) and purges orphaned
//! rows by key range before re-populating.

use heliosdb_nano::{EmbeddedDatabase, Value};

const DATA: &str = "/tmp/td-j-work";

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int8(n) => *n,
        Value::Int4(n) => i64::from(*n),
        Value::Int2(n) => i64::from(*n),
        Value::Float8(f) => *f as i64,
        Value::Float4(f) => *f as i64,
        Value::Numeric(s) | Value::String(s) => s.parse::<f64>().map(|x| x as i64).unwrap_or(-1),
        other => panic!("expected numeric, got {other:?}"),
    }
}
fn scalar(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let (rows, _) = db.query_with_columns(sql).expect(sql);
    as_i64(rows[0].values.first().expect("a value"))
}
fn mk(db: &EmbeddedDatabase, name: &str, body: &str) {
    let _ = db.execute(&format!("DROP MATERIALIZED VIEW {name}"));
    db.execute(&format!("CREATE MATERIALIZED VIEW {name} AS {body}"))
        .unwrap_or_else(|e| panic!("create {name}: {e}"));
}

#[test]
fn quirk_j_mv_aggregates_against_real_data() {
    if !std::path::Path::new(DATA).exists() {
        eprintln!("SKIP: {DATA} not present");
        return;
    }
    let db = EmbeddedDatabase::new(DATA).expect("open data dir");

    // Content guard, not just a path guard: /tmp cleaners have hollowed this
    // fixture out before (dir survives, SSTs gone), and RocksDB re-initializes
    // an empty DB in the husk on open — the path check then passes while the
    // 448k-row dataset this repro is ABOUT is absent. An empty husk proves
    // nothing either way, so skip unless the real table is present.
    if db.query("SELECT COUNT(*) FROM dashboard.messages", &[]).is_err() {
        eprintln!("SKIP: {DATA} exists but does not contain the Token-Dashboard dataset (hollowed fixture)");
        return;
    }

    // Ground truth via direct queries.
    let d_count = scalar(&db, "SELECT COUNT(*) FROM dashboard.messages");
    let d_distinct = scalar(&db, "SELECT COUNT(DISTINCT session_id) FROM dashboard.messages");
    let d_sum = scalar(&db, "SELECT SUM(input_tokens) FROM dashboard.messages");
    let (d_grp, _) = db
        .query_with_columns("SELECT type, COUNT(*) AS n FROM dashboard.messages GROUP BY type")
        .expect("direct group");
    assert_eq!(d_count, 448_573);
    assert_eq!(d_distinct, 265);

    // MV: COUNT(DISTINCT) — the headline repro (was 4). This name also has an orphaned
    // `__mv_td_distinct` data row in the delivered dir, so it exercises the purge too.
    mk(
        &db,
        "td_distinct",
        "SELECT COUNT(DISTINCT session_id) AS n FROM dashboard.messages",
    );
    assert_eq!(
        scalar(&db, "SELECT * FROM td_distinct"),
        d_distinct,
        "MV COUNT(DISTINCT)"
    );

    // MV: COUNT(*) + SUM (was 407 / 353).
    mk(
        &db,
        "td_two",
        "SELECT COUNT(*) AS n, SUM(input_tokens) AS s FROM dashboard.messages",
    );
    let (two, _) = db.query_with_columns("SELECT * FROM td_two").unwrap();
    assert_eq!(as_i64(&two[0].values[0]), d_count, "MV COUNT(*) in multi-agg");
    assert_eq!(as_i64(&two[0].values[1]), d_sum, "MV SUM");

    // MV: GROUP BY (was 4 groups / a partial total).
    mk(
        &db,
        "td_group",
        "SELECT type, COUNT(*) AS n FROM dashboard.messages GROUP BY type",
    );
    let (grp, _) = db.query_with_columns("SELECT * FROM td_group").unwrap();
    let grp_total: i64 = grp.iter().map(|r| as_i64(r.values.last().unwrap())).sum();
    assert_eq!(grp.len(), d_grp.len(), "MV GROUP BY group count");
    assert_eq!(grp_total, d_count, "MV GROUP BY total rows");

    // REFRESH must keep the correct value (the refresh path had the same bug).
    db.execute("REFRESH MATERIALIZED VIEW td_distinct").expect("refresh");
    assert_eq!(scalar(&db, "SELECT * FROM td_distinct"), d_distinct, "after REFRESH");

    // And it must persist across a reopen (the wrong value used to be on disk).
    drop(db);
    let db = EmbeddedDatabase::new(DATA).expect("reopen");
    assert_eq!(scalar(&db, "SELECT * FROM td_distinct"), d_distinct, "after reopen");

    eprintln!("Quirk J fixed: all MV aggregate forms match direct queries on 448k rows.");
}
