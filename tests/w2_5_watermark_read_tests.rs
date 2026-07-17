//! W2.5 — per-table committed-write watermark for in-transaction reads.
//!
//! The optimization lets an in-transaction reader of a table that has had no
//! committed write since its snapshot serve rows straight from current `data:`
//! instead of the per-row `scan_table_at_snapshot` version probe. It MUST be
//! behavior-preserving, so these tests pin the snapshot-isolation / read-your-
//! own-writes / DDL invariants that the fast path must not change. They pass on
//! both the pre- and post-change engine (a behavior-preserving optimization has
//! no result-level flip); their value is as regression guards — in particular
//! `repeatable_read_full_scan_hides_commit_after_snapshot` FAILS if the
//! commit-path watermark bump is dropped (the reader would then wrongly
//! fast-path and observe a commit newer than its snapshot).

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

fn ids(rows: &[heliosdb_nano::Tuple]) -> Vec<i32> {
    let mut out: Vec<i32> = rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::Int4(id)) => Some(*id),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out
}

/// An in-transaction full scan of an unchanged table returns exactly the
/// autocommit result (the fast path must be row-for-row identical to the
/// snapshot path).
#[test]
fn in_txn_full_scan_matches_autocommit_when_unchanged() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    let in_txn = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&in_txn), vec![1, 2], "in-txn scan must see the committed rows");

    // A filtered (pushdown) scan takes the FilteredScan fast path.
    let filtered = db.query_in_session(a, "SELECT * FROM t WHERE v >= 150", &[]).unwrap();
    assert_eq!(ids(&filtered), vec![2], "in-txn filtered scan must match the predicate");

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();
}

/// Snapshot isolation across the watermark fast/slow decision: a RepeatableRead
/// reader whose snapshot predates another session's commit must NOT observe
/// that commit on a later full scan (the watermark bump forces the snapshot
/// path). This is the load-bearing guard — it fails if the commit-path bump is
/// removed, because the reader would keep fast-reading current `data:`.
#[test]
fn repeatable_read_full_scan_hides_commit_after_snapshot() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    // Establish A's snapshot with a first read (fast-path eligible: the setup
    // inserts committed before A began, so t's watermark <= A's snapshot).
    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&first), vec![1, 2]);

    // Another session commits a new row AFTER A's snapshot.
    let b = db.create_session("b", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(b).unwrap();
    db.execute_in_session(b, "INSERT INTO t (id, v) VALUES (3, 300)")
        .unwrap();
    db.commit_transaction_for_session(b).unwrap();
    db.destroy_session(b).unwrap();

    // A re-reads: snapshot isolation must still hide row 3.
    let second = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        ids(&second),
        vec![1, 2],
        "RepeatableRead reader must not observe a commit newer than its snapshot"
    );

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    // A fresh autocommit read DOES observe the committed row.
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&fresh), vec![1, 2, 3]);
}

/// The reader's own uncommitted writes override the fast path (read-your-own-
/// writes): the per-table write-set gate keeps the read on the merging path.
#[test]
fn in_txn_scan_sees_own_uncommitted_writes() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    db.execute_in_session(a, "INSERT INTO t (id, v) VALUES (9, 900)")
        .unwrap();
    let seen = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        ids(&seen),
        vec![1, 2, 9],
        "the transaction must read its own staged insert"
    );

    // A concurrent session must not see A's uncommitted row.
    let other = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&other), vec![1, 2]);

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();
    let after = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&after), vec![1, 2, 9]);
}

/// A ReadCommitted reader refreshes its snapshot per statement, so it observes
/// a row another session commits between two of its statements — the fast path
/// must preserve this (its refreshed snapshot is >= the commit, so the current
/// `data:` image it serves includes the row).
#[test]
fn read_committed_full_scan_sees_commit_between_statements() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(a).unwrap();
    let before = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&before), vec![1, 2]);

    let b = db.create_session("b", IsolationLevel::ReadCommitted).unwrap();
    db.begin_transaction_for_session(b).unwrap();
    db.execute_in_session(b, "INSERT INTO t (id, v) VALUES (4, 400)")
        .unwrap();
    db.commit_transaction_for_session(b).unwrap();
    db.destroy_session(b).unwrap();

    let after = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        ids(&after),
        vec![1, 2, 4],
        "ReadCommitted must observe a row committed before this statement"
    );

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();
}

/// Sorted `(id, v)` pairs, for value-level assertions where an UPDATE changes a
/// row that stays present in `data:` (so `ids` alone would not reveal the flip).
fn pairs(rows: &[heliosdb_nano::Tuple]) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = rows
        .iter()
        .filter_map(|r| match (r.values.first(), r.values.get(1)) {
            (Some(Value::Int4(id)), Some(Value::Int4(v))) => Some((*id, *v)),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out
}

/// A RepeatableRead reader whose snapshot predates a concurrent *autocommit
/// parameterized* UPDATE must not observe it. While the reader's session
/// transaction is open the fast UPDATE path bails (`any_session_txns`), so the
/// statement routes through the generic `update_tuples_branch_aware` storage
/// funnel — which writes an un-indexed `v:` version, so it invalidates the
/// table's committed-write watermark (dropping the table to the default-closed
/// snapshot path) rather than bumping it. FAILS on pre-change code (no watermark
/// maintenance in that funnel): the reader fast-reads current `data:` and its
/// re-read sees `(1, 999)`; the snapshot path resolves the pre-update `(1, 100)`.
#[test]
fn repeatable_read_hides_autocommit_param_update_after_snapshot() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    // First read establishes A's snapshot and is fast-path eligible (the setup
    // inserts bumped t's watermark before A began).
    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&first), vec![(1, 100), (2, 200)]);

    // Concurrent autocommit parameterized UPDATE. With A open the fast path
    // bails, so this exercises the generic un-staged update funnel.
    db.execute_params("UPDATE t SET v = $1 WHERE id = $2", &[Value::Int4(999), Value::Int4(1)])
        .unwrap();

    // Snapshot isolation: A must still resolve the pre-update value.
    let second = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        pairs(&second),
        vec![(1, 100), (2, 200)],
        "RepeatableRead reader must not observe an autocommit UPDATE committed after its snapshot"
    );

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    // A fresh autocommit read DOES observe the update.
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&fresh), vec![(1, 999), (2, 200)]);
}

/// A RepeatableRead reader whose snapshot predates a concurrent *batched*
/// autocommit INSERT (multi-row `VALUES` — the same storage funnel COPY uses,
/// `insert_prepared_tuples_fast_batch`) must not observe the new rows. With
/// time-travel on this batch path is NOT blocked while a session transaction is
/// open, so it can commit rows the reader must not see; the snapshot path gates
/// them out by their `vmeta:` marker / newer version. FAILS on pre-change code:
/// without the batch-path watermark bump the reader fast-reads current `data:`
/// and its re-read includes rows 3 and 4.
#[test]
fn repeatable_read_hides_autocommit_batch_insert_after_snapshot() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&first), vec![(1, 100), (2, 200)]);

    // Concurrent autocommit multi-row INSERT routes through the batched fast
    // path (insert_prepared_tuples_fast_batch) while A is open.
    db.execute_many_params(
        "INSERT INTO t (id, v) VALUES ($1, $2)",
        &[
            vec![Value::Int4(3), Value::Int4(300)],
            vec![Value::Int4(4), Value::Int4(400)],
        ],
    )
    .unwrap();

    let second = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        pairs(&second),
        vec![(1, 100), (2, 200)],
        "RepeatableRead reader must not observe a batch INSERT committed after its snapshot"
    );

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&fresh), vec![(1, 100), (2, 200), (3, 300), (4, 400)]);
}

/// A RepeatableRead reader must not observe a concurrent autocommit
/// `INSERT ... ON CONFLICT DO UPDATE` that updates an existing row. This routes
/// through the generic INSERT arm (the fast paths bail while a session
/// transaction is open) and applies the change via the *version-skipping*
/// `update_tuple_fast`, which overwrites `data:` without recording a version and
/// therefore invalidates the table's committed-write watermark. FAILS on
/// pre-change code (no watermark maintenance): the reader fast-reads current
/// `data:` and its re-read sees `(1, 999)`; the snapshot path resolves the
/// pre-update `(1, 100)`.
#[test]
fn repeatable_read_hides_autocommit_on_conflict_update_after_snapshot() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&first), vec![(1, 100), (2, 200)]);

    // Autocommit upsert that conflicts on id = 1 and updates it in place.
    db.execute_params(
        "INSERT INTO t (id, v) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
        &[Value::Int4(1), Value::Int4(999)],
    )
    .unwrap();

    let second = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        pairs(&second),
        vec![(1, 100), (2, 200)],
        "RepeatableRead reader must not observe an autocommit ON CONFLICT DO UPDATE after its snapshot"
    );

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&fresh), vec![(1, 999), (2, 200)]);
}

/// DDL clears the watermark map (via the schema-generation bump), resetting
/// every table to the default-closed snapshot path. A read after a concurrent
/// DDL must still return correct rows.
#[test]
fn ddl_clears_watermark_but_reads_stay_correct() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    // First read makes t fast-path eligible.
    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&first), vec![1, 2]);

    // A concurrent DDL bumps the schema generation, clearing all watermarks.
    db.execute("CREATE TABLE other (k INTEGER PRIMARY KEY)").unwrap();

    // t is unchanged; the read is correct whether it takes the (now
    // default-closed) snapshot path or a re-established fast path.
    let second = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(ids(&second), vec![1, 2]);

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();
}

/// MERGE BRANCH into main writes branch rows straight into main's `data:`
/// keyspace outside every tracked commit funnel — no version, and (crucially)
/// no branch-context switch, so the watermark/existence invalidation that a
/// `USE BRANCH` would trigger never fires. A RepeatableRead reader open across
/// the merge, whose watermark gate was open on `t`, must NOT observe the merged
/// row values: the merge must invalidate the committed-write watermark so the
/// reader drops to the snapshot path (which resolves the pre-merge version).
/// FAILS on pre-change code: the merge leaves the watermark in place, so the
/// reader keeps fast-reading current `data:` and its re-read sees `(1, 999)`.
#[test]
fn merge_into_main_hidden_from_open_snapshot_reader() {
    let db = setup(); // t: (1,100),(2,200)

    // A branch that updates row 1 in isolation (writes only `bdata:`; main's
    // `data:`/`v:` for row 1 stay at the inserted 100).
    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    db.execute("USE BRANCH b").unwrap();
    db.execute("UPDATE t SET v = 999 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();

    // The branch switches cleared t's watermark; a fresh main insert (versioned)
    // re-establishes it so the about-to-open reader is fast-path eligible.
    db.execute("INSERT INTO t (id, v) VALUES (3, 300)").unwrap();

    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();
    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&first), vec![(1, 100), (2, 200), (3, 300)]);

    // Merge branch b into main: copies bdata:{b}:t:1 -> data:t:1 = 999, with no
    // branch-context switch.
    db.execute("MERGE BRANCH b INTO main").unwrap();

    // Snapshot isolation: A's re-read must still resolve the pre-merge row 1.
    let second = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(
        pairs(&second),
        vec![(1, 100), (2, 200), (3, 300)],
        "a reader open across a MERGE into main must not observe the merged row values"
    );

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    // A fresh autocommit read DOES observe the merged value (and no stale row
    // cache entry survives the merge).
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&fresh), vec![(1, 999), (2, 200), (3, 300)]);
}

/// ALTER TABLE ADD COLUMN backfills every `data:` row via a raw `db.put`
/// (`add_column_to_rows`) with NO snapshot-resolvable version and outside every
/// tracked commit funnel — the same one-directional risk as a MERGE into main.
/// The caller path (`update_table_schema`) does not bump the schema generation,
/// so nothing else fences the row rewrite. A RepeatableRead reader open across
/// the ALTER, whose watermark gate was open on `t`, must NOT observe the
/// backfilled column value: the rewrite must invalidate `t`'s committed-write
/// watermark so the reader drops to the snapshot path (which today errors on
/// schema-evolved old versions — a pre-existing limitation filed for Wave 3 —
/// and after that fix will project the absent column as NULL). FAILS on
/// pre-change code: the rewrite leaves the watermark in place, so the reader
/// keeps fast-reading current `data:` and its re-read sees the backfilled 42.
#[test]
fn alter_add_column_hidden_from_open_snapshot_reader() {
    let db = setup(); // t: (1,100),(2,200); the setup inserts established t's watermark
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();

    // First read establishes A's snapshot and is fast-path eligible.
    let first = db.query_in_session(a, "SELECT * FROM t", &[]).unwrap();
    assert_eq!(pairs(&first), vec![(1, 100), (2, 200)]);

    // Concurrent autocommit ALTER backfills every row with c = 42 (raw put, no
    // version, no watermark bump on pre-change code).
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 42").unwrap();

    // The property this pins: the open reader must NEVER observe the
    // backfilled 42 through the watermark fast path (pre-fix it did — the
    // rewrite left the watermark in place and the fast path served the
    // rewritten `data:` tuples). Post-fix the ALTER funnel
    // (catalog::update_table_schema) bumps the schema generation, clearing
    // the watermark, and the reader drops to the snapshot path.
    //
    // KNOWN PRE-EXISTING LIMITATION (filed for Wave 3, NOT a W2.5 artifact —
    // the error sites predate this wave: evaluator.rs "Column index N out of
    // bounds in tuple"): the snapshot path projects the CURRENT catalog over
    // pre-ALTER 2-value version tuples and errors instead of NULL-padding.
    // Blanket NULL-padding is not the fix (that error legitimately catches
    // planner projection bugs); proper version-aware schema resolution is the
    // Wave-3 item. So accept either outcome that does NOT leak the backfill:
    // a clean error (today) or, once Wave 3 lands, rows with c IS NULL.
    let second = db.query_in_session(a, "SELECT * FROM t", &[]);
    match second {
        Err(_) => {} // snapshot path, pre-existing schema-evolution limitation
        Ok(rows) => {
            let c_values: Vec<Value> = rows.iter().map(|r| r.get(2).cloned().unwrap_or(Value::Null)).collect();
            assert!(
                c_values.iter().all(|c| *c == Value::Null),
                "a reader open across ALTER ADD COLUMN must not observe the backfilled column (got {:?})",
                c_values
            );
        }
    }

    db.commit_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    // A fresh autocommit read DOES observe the backfilled column (confirming the
    // ALTER really rewrote `data:`, so the pre-change fast path WOULD have leaked
    // the value to A).
    let fresh = db.query("SELECT * FROM t", &[]).unwrap();
    let fresh_c: Vec<Value> = fresh.iter().map(|r| r.get(2).cloned().unwrap_or(Value::Null)).collect();
    assert!(
        fresh_c.len() == 2 && fresh_c.iter().all(|c| *c != Value::Null),
        "a fresh autocommit read must observe the backfilled column (got {:?})",
        fresh_c
    );
}

/// A RepeatableRead reader that begins AFTER a concurrent autocommit
/// general-funnel UPDATE (`update_tuples_branch_aware`, which writes an
/// un-indexed `v:` version) must read a CONSISTENT value for the updated row
/// across its statements, even when its own later write to the table flips it
/// off the watermark fast path onto the snapshot path. Because the funnel
/// invalidates the table's watermark (rather than bumping it), BOTH reads take
/// the snapshot path and agree. FAILS on the pre-fix bump: the first read
/// fast-reads the (newer) `data:` value while the second — forced onto the
/// snapshot path by the reader's own write — resolves the older snapshot
/// version, a non-repeatable read inside one RepeatableRead transaction.
///
/// The value both reads resolve is intentionally the pre-update snapshot value
/// (the un-indexed-autocommit-version limitation that `version_gc_tests.rs`'s
/// C15 chain rule preserves); this test pins REPEATABILITY, not currency, so it
/// remains valid if a future change makes the funnel write indexed versions.
#[test]
fn general_funnel_update_repeatable_read_stays_consistent() {
    let db = setup(); // t: (1,100),(2,200)

    // Keeping a session transaction open forces the UPDATE off the fast path
    // (bails on `any_session_txns`) and onto the generic un-indexed funnel
    // (`update_tuples_branch_aware`), which invalidates t's watermark. The
    // holder writes nothing, so its commit does not re-establish the watermark.
    let holder = db.create_session("holder", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(holder).unwrap();
    db.execute_params("UPDATE t SET v = $1 WHERE id = $2", &[Value::Int4(999), Value::Int4(1)])
        .unwrap();
    db.commit_transaction_for_session(holder).unwrap();
    db.destroy_session(holder).unwrap();

    // A RepeatableRead reader whose snapshot begins AFTER the update.
    let c = db.create_session("c", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(c).unwrap();

    // Full scan routes through the in-txn watermark gate (a PK point read could
    // take an index-probe path that always uses the snapshot path).
    let first = db.query_in_session(c, "SELECT * FROM t", &[]).unwrap();
    let v_first = pairs(&first);

    // C's own write to t flips it onto the snapshot (merging) path for the next read.
    db.execute_in_session(c, "INSERT INTO t (id, v) VALUES (5, 500)")
        .unwrap();

    // Exclude C's own inserted row 5; the pre-existing rows must be unchanged.
    let second = db.query_in_session(c, "SELECT * FROM t", &[]).unwrap();
    let v_second: Vec<(i32, i32)> = pairs(&second).into_iter().filter(|(id, _)| *id != 5).collect();

    assert_eq!(
        v_first, v_second,
        "the pre-existing rows must not change across statements in one RepeatableRead txn"
    );

    db.commit_transaction_for_session(c).unwrap();
    db.destroy_session(c).unwrap();
}
