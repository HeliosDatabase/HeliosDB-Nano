//! W3.5 — characterization of snapshot / AS OF / branch reads of rows written
//! under an OLDER schema than the current catalog (after ALTER ADD COLUMN).
//!
//! These tests PIN TODAY'S behavior (design-first item: no engine change ships
//! with them). `ALTER TABLE t ADD COLUMN c DEFAULT 42` rewrites every `data:`
//! row in place with no snapshot-resolvable version and bumps the schema
//! generation (`catalog::update_table_schema` -> `bump_schema_generation`), so
//! W2.5 fail-closes an in-transaction reader onto the snapshot path. That path
//! resolves the pre-ALTER 2-value version and projects it under the current
//! 3-column catalog (`SELECT *` expands `Wildcard` to one explicit column ref
//! per current column, `planner.rs`), so column index 2 is out of bounds:
//! `evaluator.rs` / `project.rs` "Column index N out of bounds in tuple". The
//! guard predates this campaign (evaluator.rs Column guard `484460e`
//! 2026-02-04) and legitimately catches planner arity bugs (v334_t8, `e8e905d`)
//! on intermediate tuples, so blanket NULL-padding it is wrong — the W3.5 design
//! (`docs/plans/PERF_STABILITY_2026_07/W3_5_DESIGN.md`) pads only at the
//! base-table scan boundary.
//!
//! Each test documents which assertion the design would flip. The error-shape
//! tests use the w2_5 defensive `match`: the `Err` arm (today's path) pins the
//! "out of bounds" message; the `Ok` arm — reached only once the design lands,
//! or if `SELECT *` returns the raw short tuples rather than projecting — pins
//! the invariant that SURVIVES the flip: a pre-ALTER snapshot must NEVER expose
//! the `42` backfill (an isolation break), only absence (NULL / a missing
//! trailing value).

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 100)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (2, 200)").unwrap();
    db
}

fn now_sql_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn sleep_secs(secs: f64) {
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int2(n) => Some(*n as i64),
        Value::Int4(n) => Some(*n as i64),
        Value::Int8(n) => Some(*n),
        _ => None,
    }
}

/// A row exposes the ADD COLUMN backfill iff its 3rd value is a non-NULL `42`.
/// A missing 3rd value (a raw 2-value tuple) or an explicit NULL is absence, not
/// a leak.
fn exposes_backfill(row: &heliosdb_nano::Tuple) -> bool {
    matches!(row.get(2), Some(v) if as_i64(v) == Some(42))
}

/// The id (first value) of a row, for locating a specific forked row.
fn row_id(row: &heliosdb_nano::Tuple) -> Option<i64> {
    row.values.first().and_then(as_i64)
}

/// Open RepeatableRead transaction whose snapshot predates a concurrent ALTER
/// ADD COLUMN: the re-read routes through the snapshot path (the ALTER cleared
/// the watermark). TODAY: errors "Column index 2 out of bounds in tuple" — the
/// snapshot resolves the pre-ALTER 2-value versions and the plan projects the
/// current 3-column catalog over them. NEVER exposes the `42` backfill.
///
/// DESIGN FLIP: Stage 1 returns the rows with `c IS NULL`; Stage 2 returns the
/// 2-column pre-ALTER shape.
#[test]
fn open_txn_read_after_alter_add_column_today() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    // Establish A's snapshot before the ALTER (both rows committed pre-ALTER, so
    // the snapshot resolves their 2-value versions).
    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(first.len(), 2, "in-txn scan must see the two committed rows");

    // Concurrent autocommit ALTER: rewrites data: to 3 values (c = 42) with no
    // version, and bumps the schema generation (clears A's watermark).
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    let second = db.query_in_session(a, "SELECT * FROM t", &[]);
    match second {
        Err(e) => {
            // TODAY: the snapshot path errors on the schema-evolved rows.
            assert!(
                e.to_string().contains("out of bounds"),
                "expected an arity ('out of bounds') error on the pre-ALTER snapshot, got: {e}"
            );
        }
        Ok(rows) => {
            // Only reachable once the design lands (or if SELECT * returns the
            // raw 2-value tuples). The surviving invariant: no backfill leak.
            assert!(
                rows.iter().all(|r| !exposes_backfill(r)),
                "a reader whose snapshot predates the ALTER must never observe the 42 backfill"
            );
        }
    }

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    // A fresh autocommit read DOES observe the backfilled column (confirming the
    // ALTER really rewrote data:, so a leak WOULD have been observable).
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(
        fresh.len() == 2 && fresh.iter().all(exposes_backfill),
        "a fresh autocommit read must observe c = 42 on every row"
    );
}

/// Autocommit `SELECT * ... AS OF TIMESTAMP '<before the ALTER>'`: the SAME
/// problem class as the in-txn snapshot read, no session transaction needed. The
/// AS OF branch resolves to a pre-ALTER snapshot and calls the same
/// `scan_table_at_snapshot` against a plan built from the current 3-column
/// catalog. TODAY: errors "out of bounds"; NEVER exposes the `42` backfill.
///
/// DESIGN FLIP: the shared base-scan fix flips this identically to the in-txn
/// case (one fix covers both — the design's key advantage).
#[test]
fn as_of_read_predating_alter_today() {
    let db = setup();

    // Capture a timestamp strictly after the inserts (second-granularity AS OF
    // resolution needs a >= 1s gap, matching version_gc_tests) and before the
    // ALTER, so it resolves the pre-ALTER 2-value versions.
    sleep_secs(1.2);
    let ts_before_alter = now_sql_timestamp();

    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    let historical = db.query(
        &format!("SELECT * FROM t AS OF TIMESTAMP '{}'", ts_before_alter),
        &[],
    );
    match historical {
        Err(e) => {
            assert!(
                e.to_string().contains("out of bounds"),
                "expected an arity ('out of bounds') error on the pre-ALTER AS OF read, got: {e}"
            );
        }
        Ok(rows) => {
            assert!(
                rows.iter().all(|r| !exposes_backfill(r)),
                "an AS OF read predating the ALTER must never observe the 42 backfill"
            );
        }
    }

    // AS OF NOW / the live read sees the backfill (sanity: the ALTER landed).
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(
        fresh.len() == 2 && fresh.iter().all(exposes_backfill),
        "the current read must observe c = 42 on every row"
    );
}

/// A branch that FORKED a row (wrote `bdata:`, 2-value) before a main ALTER: the
/// branch overlay yields that 2-value row under the current 3-column catalog, so
/// reading the branch hits the same arity error on the forked row. TODAY: errors
/// "out of bounds"; the forked row NEVER exposes the `42` backfill.
///
/// DESIGN FLIP (Stage 1 applied to the bdata: decode): the forked row reads with
/// `c IS NULL`. The un-forked row (row 2) legitimately shows main's backfilled
/// 42 — that is the branch-overlay model, pinned separately below.
#[test]
fn branch_forked_row_before_alter_today() {
    let db = setup();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    // Fork row 1 under the 2-column schema (bdata:{b}:t:1 = (1, 111)).
    db.execute("UPDATE t SET v = 111 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();

    // Main ALTER rewrites main's data: to 3 values; b's bdata: row 1 is NOT
    // touched, so it stays 2-value.
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let branch_read = db.query("SELECT * FROM t", &[]);
    match branch_read {
        Err(e) => {
            assert!(
                e.to_string().contains("out of bounds"),
                "expected an arity error reading the forked 2-value row under the 3-col catalog, got: {e}"
            );
        }
        Ok(rows) => {
            // The forked row (id = 1, v = 111) must not expose the backfill.
            let forked = rows
                .iter()
                .find(|r| row_id(r) == Some(1))
                .expect("branch must still see forked row 1");
            assert_eq!(
                forked.values.get(1).and_then(as_i64),
                Some(111),
                "the forked row must carry the branch's own value"
            );
            assert!(
                !exposes_backfill(forked),
                "the forked row (written pre-ALTER) must not expose the main ALTER's 42 backfill"
            );
        }
    }
}

/// A branch created before a main ALTER, with NO fork of the row: the overlay
/// reads main's LIVE (rewritten, 3-value) data: for un-forked rows, so the
/// branch DOES observe the backfilled 42. This is the copy-on-write branch model
/// (a branch is not snapshot-isolated from main DDL for rows it has not forked),
/// pinned as-is. TODAY: Ok, both rows show c = 42.
///
/// NOT FLIPPED by the W3.5 tuple-decode fix (3 values match the 3-col catalog —
/// no arity mismatch); insulating a pre-ALTER branch from a main ALTER would be a
/// separate per-branch schema pin (design §7, out of scope).
#[test]
fn branch_before_alter_sees_main_backfill_today() {
    let db = setup();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    // ALTER on main while b makes no writes.
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 2, "branch must see both un-forked rows via the main overlay");
    assert!(
        rows.iter().all(exposes_backfill),
        "an un-forked branch reads main's live rewritten data:, so it observes c = 42"
    );
}
