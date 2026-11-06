//! W3.5 — snapshot / AS OF / branch reads of rows written under an OLDER schema
//! than the current catalog (after ALTER ADD COLUMN).
//!
//! Spec R5.2: `ALTER TABLE t ADD COLUMN c DEFAULT 42` rewrites no row. It stores
//! the column's MISSING VALUE beside the schema and bumps the schema generation,
//! so W2.5 fail-closes an in-transaction reader onto the snapshot path. That path
//! resolves the 2-value version and the plan (built from the current 3-column
//! catalog: `SELECT *` expands `Wildcard` to one explicit column ref per current
//! column, `planner.rs`) projects column index 2.
//!
//! The base-table decode boundaries shape a row written under an older (narrower)
//! schema to the current catalog width, so index 2 reads the column's missing
//! value, or NULL when it has none, as PostgreSQL's old tuples do: the column
//! exists, and a row older than it holds its default. (W3.5 Stage 1 padded NULL
//! there because the default was a rewrite a snapshot could not see.) The shaping
//! is localized to the scan boundary, where the tuple is provably a stored base
//! row of a known table — NEVER the generic evaluator/aggregate/join arity guards,
//! which stay hard errors for mis-planned intermediate tuples (design §1.1). A row
//! WIDER than the catalog after a DROP COLUMN (a resolved old version, or a
//! `bdata:` fork the DROP left un-rewritten) is legitimate schema evolution, not
//! corruption: it is TRUNCATED to the current width, matching the pre-W3.5
//! projection.
//!
//! For a row VERSION the behavior is gated by `storage.snapshot_schema_evolution`:
//! - `"null_pad"` (default): the shaper above.
//! - `"strict"`: the pre-W3.5 arity error, kept as a rollback.
//!
//! These tests assert the Stage-1 flip under the default, the `"strict"`
//! rollback, and the wider-than-catalog truncation. The branch-overlay
//! "un-forked row sees main's backfill" case (design §7, branch-model) is
//! explicitly out of scope and stays pinned as-is.
//!
//! # The branch rule changed (sprinter 6bc4a4040eb7, phase B3)
//!
//! A branch now has its own catalog overlay (`storage::branch_catalog`): a
//! table it changed itself (`ALTER TABLE … ADD COLUMN` on the branch) has the
//! branch's OWN definition, and its rows were materialized onto the branch
//! under it — the branch no longer reads main's definition or main's rows of
//! that table, so a later ADD COLUMN on main is invisible there
//! (`branch_with_its_own_add_column_is_isolated_from_mains_add_column`, new).
//! A table the branch has NOT changed itself still reads main's CURRENT
//! definition and rows, which is why the three branch pins below keep their
//! answers in B3 — they are re-labelled as that rule, not the old global
//! catalog. Phase B4 freezes main's definition and rows into live branches
//! at main's write (copy-on-write), and will flip those three pins.

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

/// A row reads the ADD COLUMN default iff its 3rd value is a non-NULL `42`.
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
/// Spec R5.2: those rows are older than the column, so the re-read returns the
/// column's missing value, the same `42` a fresh read does — never an arity error.
#[test]
fn open_txn_read_after_alter_add_column_today() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    // Establish A's snapshot before the ALTER (both rows committed pre-ALTER, so
    // the snapshot resolves their 2-value versions).
    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(first.len(), 2, "in-txn scan must see the two committed rows");

    // Concurrent autocommit ALTER: no row changes, and the schema generation
    // moves (clears A's watermark).
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    let rows = db
        .query_in_session(a, "SELECT * FROM t", &[])
        .expect("a pre-ALTER snapshot read must succeed, not error on arity");
    assert_eq!(rows.len(), 2, "the snapshot still resolves both pre-ALTER rows");
    assert!(
        rows.iter().all(exposes_backfill),
        "a row version older than the column reads its missing value"
    );

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(
        fresh.len() == 2 && fresh.iter().all(exposes_backfill),
        "a fresh autocommit read must observe c = 42 on every row"
    );
}

/// Autocommit `SELECT * ... AS OF TIMESTAMP '<before the ALTER>'`: the SAME
/// problem class as the in-txn snapshot read, no session transaction needed. The
/// AS OF branch resolves to a pre-ALTER snapshot and calls the same
/// `scan_table_at_snapshot`, so the shared funnel answers identically: the
/// column's missing value, for a row version older than the column.
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
        .expect("a pre-ALTER AS OF read must succeed");
    assert_eq!(rows.len(), 2, "AS OF resolves both pre-ALTER rows");
    assert!(
        rows.iter().all(exposes_backfill),
        "a row version older than the column reads its missing value"
    );

    // AS OF NOW / the live read sees the backfill (sanity: the ALTER landed).
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert!(
        fresh.len() == 2 && fresh.iter().all(exposes_backfill),
        "the current read must observe c = 42 on every row"
    );
}

/// A branch that FORKED a row (wrote `bdata:`, 2-value) before a main ALTER: the
/// branch overlay yields that 2-value row under the current 3-column catalog.
///
/// Spec R5.2 applied to the `bdata:` decode: the forked row is older than the
/// column and reads its missing value (no arity error), like the un-forked row 2.
///
/// sprinter 6bc4a4040eb7 (phase B3): unchanged — forking a ROW is not a change
/// of the table's definition, so the branch still reads main's current
/// definition of `t` (the B3 rule; B4 freezes main's pre-image instead).
#[test]
fn branch_forked_row_before_alter_today() {
    let db = setup();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    // Fork row 1 under the 2-column schema (bdata:{b}:t:1 = (1, 111)).
    db.execute("UPDATE t SET v = 111 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();

    // Main's ALTER touches no row; b's bdata: row 1 stays 2-value.
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db
        .query("SELECT * FROM t", &[])
        .expect("the forked 2-value row is shaped, no arity error");

    // The forked row (id = 1, v = 111) keeps the branch's own value and reads the
    // column's missing value.
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
        exposes_backfill(forked),
        "the forked row (written pre-ALTER) reads the column's missing value"
    );

    // The un-forked row (id = 2) reads main's row and the same missing value — the
    // branch-overlay model (design §7).
    let unforked = rows
        .iter()
        .find(|r| row_id(r) == Some(2))
        .expect("branch must see un-forked row 2 via the main overlay");
    assert!(
        exposes_backfill(unforked),
        "an un-forked row reads main's data: under main's definition, so it observes c = 42"
    );
}

/// A branch created before a main ALTER, with NO fork of the row: the overlay
/// reads main's LIVE data: for un-forked rows under main's definition, so the
/// branch DOES observe the 42. This is the copy-on-write branch model
/// (a branch is not snapshot-isolated from main DDL for rows it has not forked),
/// pinned as-is. TODAY: Ok, both rows show c = 42.
///
/// NOT FLIPPED by the W3.5 tuple-decode fix (3 values match the 3-col catalog —
/// no arity mismatch); insulating a pre-ALTER branch from a main ALTER would be a
/// separate per-branch schema pin (design §7, out of scope).
///
/// sprinter 6bc4a4040eb7 (phase B3): still the answer, now as the B3 rule — the
/// branch has no definition of `t` of its own, so it reads main's current one.
/// Phase B4 (main's DDL freezes the pre-image into live branches) flips it.
#[test]
fn branch_before_alter_sees_main_backfill_today() {
    let db = setup();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    // ALTER on main while b makes no writes.
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "branch must see both un-forked rows via the main overlay"
    );
    assert!(
        rows.iter().all(exposes_backfill),
        "an un-forked branch reads main's data: under main's definition, so it observes c = 42"
    );
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

/// A stored row WIDER than the current catalog after a DROP COLUMN is a
/// legitimately-old/forked row, NOT corruption: `drop_column_from_rows` rewrites
/// `data:` in place with no new version and leaves a branch's `bdata:` fork
/// untouched, so a wider row survives. It is TRUNCATED to the current width —
/// exactly the pre-W3.5 projection (the plan, built from the current catalog,
/// only references the surviving columns and silently ignored trailing values),
/// never a hard error (design §3). A branch forks a row under the 3-column schema
/// (`bdata:` = 3-value); main then DROPs a column, narrowing the (global) catalog
/// to 2 columns and rewriting main's `data:` — but NOT `bdata:`. Reading the
/// branch decodes the 3-value `bdata:` row under the 2-column catalog: A(3) >
/// W(2) → truncate to 2. (Mirror of `branch_forked_row_before_alter` with DROP
/// instead of ADD, so the retained forked row is wider, not narrower.)
#[test]
fn wider_than_catalog_row_truncates() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, c INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t (id, v, c) VALUES (1, 100, 9)").unwrap();
    db.execute("INSERT INTO t (id, v, c) VALUES (2, 200, 9)").unwrap();

    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    // Fork row 1 under the 3-column schema (bdata:{b}:t:1 = (1, 111, 9)).
    db.execute("UPDATE t SET v = 111 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();

    // Main DROP narrows the global catalog to 2 columns and rewrites main's
    // data:, but leaves b's bdata: row 1 at its 3-value arity.
    db.execute("ALTER TABLE t DROP COLUMN c").unwrap();

    db.execute("USE BRANCH b").unwrap();
    let rows = db
        .query("SELECT * FROM t", &[])
        .expect("a bdata: row wider than the current catalog is truncated, not a hard error");
    assert_eq!(rows.len(), 2, "the branch still sees both rows");

    // The forked row (id = 1) keeps the branch's own value and is truncated to
    // the current 2-column width — the dropped column's stored 9 is NOT exposed.
    let forked = rows
        .iter()
        .find(|r| row_id(r) == Some(1))
        .expect("branch must still see forked row 1");
    assert_eq!(
        forked.values.get(1).and_then(as_i64),
        Some(111),
        "the forked row must carry the branch's own value"
    );
    assert_eq!(
        forked.values.len(),
        2,
        "the wider forked row must be truncated to the current 2-column width"
    );
    assert!(
        forked.get(2).is_none(),
        "the dropped column must not be exposed after truncation"
    );

    // The un-forked row (id = 2) reads main's rewritten 2-value data: directly.
    let unforked = rows
        .iter()
        .find(|r| row_id(r) == Some(2))
        .expect("branch must see un-forked row 2 via the main overlay");
    assert_eq!(
        unforked.values.get(1).and_then(as_i64),
        Some(200),
        "the un-forked row reads main's post-DROP value"
    );
}
