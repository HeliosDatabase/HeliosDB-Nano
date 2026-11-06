//! W3.5 — snapshot / AS OF / branch reads of rows written under an OLDER schema
//! than the current catalog (after ALTER ADD COLUMN).
//!
//! `ALTER TABLE t ADD COLUMN c DEFAULT 42` rewrites every `data:` row in place
//! with no snapshot-resolvable version and bumps the schema generation
//! (`catalog::update_table_schema` -> `bump_schema_generation`), so W2.5
//! fail-closes an in-transaction reader onto the snapshot path. That path
//! resolves the pre-ALTER 2-value version and the plan (built from the current
//! 3-column catalog: `SELECT *` expands `Wildcard` to one explicit column ref
//! per current column, `planner.rs`) projects column index 2.
//!
//! W3.5 **Stage 1** now ships (`docs/plans/PERF_STABILITY_2026_07/W3_5_DESIGN.md`
//! §3): the base-table scan decode boundaries NULL-pad a row written under an
//! older (narrower) schema up to the current catalog width, so index 2 reads as
//! NULL instead of erroring. The pad is localized to the scan boundary, where
//! the tuple is provably a stored base row of a known table — NEVER the generic
//! evaluator/aggregate/join arity guards, which stay hard errors for
//! mis-planned intermediate tuples (design §1.1). A row WIDER than the catalog
//! after a DROP COLUMN (a resolved old version) is legitimate schema evolution,
//! not corruption: it is TRUNCATED to the current width, matching the pre-W3.5
//! projection. A branch no longer reaches this path: since phase B4 it keeps the
//! definition it forked with (sprinter 6bc4a4040eb7).
//!
//! The behavior is gated by `storage.snapshot_schema_evolution`:
//! - `"null_pad"` (default): the Stage-1 shaper — a pre-ADD snapshot projects the
//!   added column as NULL and NEVER the `42` backfill (an isolation break); a
//!   pre-DROP wider row is truncated to the current width.
//! - `"strict"`: the pre-W3.5 arity error, kept as a rollback.
//!
//! These tests assert the Stage-1 flip under the default, the `"strict"`
//! rollback, and the wider-than-catalog truncation.
//!
//! # The branch rule changed (sprinter 6bc4a4040eb7, phase B3)
//!
//! A branch now has its own catalog overlay (`storage::branch_catalog`): a
//! table it changed itself (`ALTER TABLE … ADD COLUMN` on the branch) has the
//! branch's OWN definition, and its rows were materialized onto the branch
//! under it — the branch no longer reads main's definition or main's rows of
//! that table, so a later ADD COLUMN on main is invisible there
//! (`branch_with_its_own_add_column_is_isolated_from_mains_add_column`, new).
//!
//! Phase B4 (same sprinter): a table the branch has NOT changed is frozen into it
//! as main changes it: main's definition before its first catalog write, and each
//! row before main rewrites it. A branch is a fork, so the pins below that said it
//! saw main's backfill now say it does not.

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{Config, EmbeddedDatabase, SnapshotSchemaEvolution, Value};

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
/// the watermark), which resolves the pre-ALTER 2-value versions.
///
/// W3.5 Stage 1 (default `null_pad`): the snapshot path NULL-pads those rows to
/// the current 3-column catalog, so the re-read returns `c IS NULL` — never the
/// `42` backfill and never an arity error.
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

    let rows = db
        .query_in_session(a, "SELECT * FROM t", &[])
        .expect("Stage 1 null_pad: a pre-ALTER snapshot read must succeed, not error on arity");
    assert_eq!(rows.len(), 2, "the snapshot still resolves both pre-ALTER rows");
    assert!(
        rows.iter().all(|r| matches!(r.get(2), Some(Value::Null))),
        "the column added after the snapshot must project as NULL"
    );
    assert!(
        rows.iter().all(|r| !exposes_backfill(r)),
        "a reader whose snapshot predates the ALTER must never observe the 42 backfill"
    );

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
/// `scan_table_at_snapshot` — so Stage 1's shared base-scan pad flips it
/// identically (the design's key advantage): `c IS NULL`, never the backfill.
#[test]
fn as_of_read_predating_alter_today() {
    let db = setup();

    // Capture a timestamp strictly after the inserts (second-granularity AS OF
    // resolution needs a >= 1s gap, matching version_gc_tests) and before the
    // ALTER, so it resolves the pre-ALTER 2-value versions.
    sleep_secs(1.2);
    let ts_before_alter = now_sql_timestamp();

    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    let rows = db
        .query(&format!("SELECT * FROM t AS OF TIMESTAMP '{}'", ts_before_alter), &[])
        .expect("Stage 1 null_pad: a pre-ALTER AS OF read must succeed");
    assert_eq!(rows.len(), 2, "AS OF resolves both pre-ALTER rows");
    assert!(
        rows.iter().all(|r| matches!(r.get(2), Some(Value::Null))),
        "the column added after the AS OF timestamp must project as NULL"
    );
    assert!(
        rows.iter().all(|r| !exposes_backfill(r)),
        "an AS OF read predating the ALTER must never observe the 42 backfill"
    );

    // AS OF NOW / the live read sees the backfill (sanity: the ALTER landed).
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(
        fresh.len() == 2 && fresh.iter().all(exposes_backfill),
        "the current read must observe c = 42 on every row"
    );
}

/// A branch that FORKED a row (wrote `bdata:`, 2-value) before a main ALTER.
///
/// sprinter 6bc4a4040eb7 (phase B4): the branch keeps the definition it forked with,
/// because main's statement froze it into the branch before it changed anything, and
/// main's rewrite of each row it had not forked froze the row's old image. So the
/// branch reads two columns, its own value and the row as it was: no padding and no
/// backfill. (Through B3 it read main's definition: the forked row padded with NULL,
/// and the un-forked one showed main's 42.)
#[test]
fn branch_forked_row_before_alter_keeps_its_definition() {
    let db = setup();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    // Fork row 1 under the 2-column schema (bdata:{b}:t:1 = (1, 111)).
    db.execute("UPDATE t SET v = 111 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();

    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(
        rows.iter().all(|r| r.values.len() == 2),
        "the branch keeps the definition it forked with"
    );
    assert!(db.query("SELECT c FROM t", &[]).is_err(), "main's column is main's");

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
        rows.iter().any(|r| row_id(r) == Some(2)),
        "the row it did not fork, as it was"
    );
    assert!(
        !rows.iter().any(exposes_backfill),
        "nothing on the branch exposes the main ALTER's 42 backfill"
    );
}

/// A branch created before a main ALTER, with NO fork of the row.
///
/// sprinter 6bc4a4040eb7 (phase B4): the fork rule. Until B3 this pinned that the branch
/// read main's LIVE rewritten rows and so saw the backfilled 42 ("a branch is not
/// snapshot-isolated from main DDL for rows it has not forked"). Main's ALTER now freezes
/// the definition and each row it rewrites into the branch, so it sees neither.
#[test]
fn branch_before_alter_does_not_see_main_backfill() {
    let db = setup();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    // ALTER on main while b makes no writes.
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 2, "branch must see both rows");
    assert!(
        rows.iter().all(|r| r.values.len() == 2),
        "the branch keeps the definition it forked with"
    );
    db.execute("USE BRANCH main").unwrap();
    let main_rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(main_rows.iter().all(exposes_backfill), "main has its column");
}

/// sprinter 6bc4a4040eb7 (phase B3) — the NEW rule. A branch that changed the
/// table's definition itself (`ADD COLUMN x DEFAULT 7` on the branch) owns that
/// definition and a materialized copy of every row it saw; main's later
/// `ADD COLUMN c DEFAULT 42` (and main's later rows) do not reach it, and the
/// branch's column never reaches main. Before B3 the branch's ADD COLUMN
/// rewrote MAIN's catalog and rows, so main grew `x` and the branch then saw
/// main's `c` too.
#[test]
fn branch_with_its_own_add_column_is_isolated_from_mains_add_column() {
    let db = setup();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    db.execute("ALTER TABLE t ADD COLUMN x INTEGER DEFAULT 7").unwrap();
    db.execute("USE BRANCH main").unwrap();

    // Main never saw the branch's column…
    let main_rows = db.query("SELECT * FROM t ORDER BY id", &[]).unwrap();
    assert!(
        main_rows.iter().all(|r| r.values.len() == 2),
        "the branch's ADD COLUMN must not reach main's catalog or rows"
    );
    // …and now changes the table itself.
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();
    db.execute("INSERT INTO t (id, v, c) VALUES (3, 300, 42)").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db.query("SELECT * FROM t ORDER BY id", &[]).unwrap();
    assert_eq!(
        rows.iter().filter_map(row_id).collect::<Vec<_>>(),
        vec![1, 2],
        "main's later row does not appear on the branch that owns its definition of t"
    );
    for row in &rows {
        assert_eq!(row.values.len(), 3, "the branch's definition: id, v, x");
        assert_eq!(
            row.values.get(2).and_then(as_i64),
            Some(7),
            "the branch's backfill, never main's 42"
        );
    }
    assert!(
        db.query("SELECT c FROM t", &[]).is_err(),
        "main's column c is not a column of the branch's t"
    );

    db.execute("USE BRANCH main").unwrap();
    let main_rows = db.query("SELECT * FROM t ORDER BY id", &[]).unwrap();
    assert_eq!(main_rows.len(), 3);
    assert!(
        main_rows.iter().all(exposes_backfill),
        "main's own ADD COLUMN backfilled its rows"
    );
    assert!(
        db.query("SELECT x FROM t", &[]).is_err(),
        "x is the branch's column only"
    );
}

/// The `snapshot_schema_evolution = "strict"` knob restores the pre-W3.5
/// behavior as a rollback: a pre-ALTER AS OF snapshot resolves the 2-value
/// versions and the 3-column plan projects column index 2 out of bounds.
#[test]
fn strict_mode_restores_arity_error() {
    let mut config = Config::in_memory();
    config.storage.snapshot_schema_evolution = SnapshotSchemaEvolution::Strict;
    let db = EmbeddedDatabase::with_config(config).unwrap();

    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 100)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (2, 200)").unwrap();

    sleep_secs(1.2);
    let ts_before_alter = now_sql_timestamp();

    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    let err = db
        .query(&format!("SELECT * FROM t AS OF TIMESTAMP '{}'", ts_before_alter), &[])
        .expect_err("strict mode must keep the pre-W3.5 arity error");
    assert!(
        err.to_string().contains("out of bounds"),
        "strict mode must surface the 'out of bounds' arity error, got: {err}"
    );
}

/// A branch forked before main's `DROP COLUMN`.
///
/// sprinter 6bc4a4040eb7 (phase B4): the branch keeps the definition it forked with, so
/// both rows keep the column main dropped. (Through B3 it read main's definition: its
/// forked row was truncated, and the other read main's rewritten row.)
#[test]
fn a_branch_keeps_a_column_main_drops() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, c INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t (id, v, c) VALUES (1, 100, 9)").unwrap();
    db.execute("INSERT INTO t (id, v, c) VALUES (2, 200, 9)").unwrap();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    db.execute("UPDATE t SET v = 111 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();

    db.execute("ALTER TABLE t DROP COLUMN c").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 2, "the branch still sees both rows");
    for (id, v) in [(1, 111), (2, 200)] {
        let row = rows
            .iter()
            .find(|r| row_id(r) == Some(id))
            .unwrap_or_else(|| panic!("row {id}"));
        assert_eq!(row.values.len(), 3, "the definition it forked with");
        assert_eq!(row.values.get(1).and_then(as_i64), Some(v));
        assert_eq!(row.values.get(2).and_then(as_i64), Some(9));
    }

    db.execute("USE BRANCH main").unwrap();
    let main_rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(main_rows.iter().all(|r| r.values.len() == 2), "main's own DROP COLUMN");
}

/// A version wider than the current catalog (written before a `DROP COLUMN`, which
/// writes no version) is truncated to the current width on `AS OF`, not a hard error,
/// and the dropped column's stored value is not exposed.
#[test]
fn wider_than_catalog_version_truncates() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, c INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t (id, v, c) VALUES (1, 100, 9)").unwrap();
    let before = db.storage.snapshot_manager().current_scn().saturating_sub(1);

    db.execute("ALTER TABLE t DROP COLUMN c").unwrap();

    let rows = db
        .query(&format!("SELECT * FROM t AS OF SCN {before}"), &[])
        .expect("a version wider than the current catalog is truncated, not a hard error");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.len(), 2, "truncated to the current 2-column width");
    assert_eq!(rows[0].values.get(1).and_then(as_i64), Some(100));
}
