//! Storage/DML batch A — three defects that all come from trusting a structure
//! that is only a HINT.
//!
//! * **sprinter 79efe5ebda6e** — the fast DELETE paths resolve their victim's
//!   row id from the eagerly-maintained PK ART index and then delete
//!   `data:{table}:{row_id}`. The index moves at STATEMENT time, the row moves
//!   at COMMIT time, so while an open transaction has moved a key the index
//!   names a row that does not carry it — and a DELETE acts on that, destroying
//!   a row nobody asked about.
//! * **sprinter 6780488554df** — the params family's Insert arm writes each row
//!   straight to storage when it resolves no transaction, so a multi-row
//!   `INSERT … VALUES ($1,…),($4,…)` that violates UNIQUE on row N left rows
//!   1..N-1 committed. PostgreSQL fails the whole statement and persists
//!   nothing; Prisma's `createMany` is exactly this shape.
//! * **sprinter 3f8e05a39baf** — `create_pk_index` claimed `{table}_pkey` in the
//!   database-GLOBAL index map without checking who held it, evicting any index
//!   already registered under that name while its side maps went on naming it.
//!
//! The ART-level unit test for the third one lives inline in
//! `src/storage/art_manager.rs` (local convention for registry-level facts);
//! what is here is the SQL-visible consequence.

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn rows(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[]).unwrap().len()
}

/// The message shape the PG wire maps to SQLSTATE 23505 unique_violation.
fn assert_unique_violation(err: &heliosdb_nano::Error, context: &str) {
    let text = err.to_string().to_lowercase();
    assert!(
        text.contains("duplicate key") || text.contains("unique constraint") || text.contains("primary key"),
        "{context}: the error must read as a UNIQUE violation (23505 on the wire), got: {err}"
    );
}

/// Two committed rows under a single-column INTEGER primary key — the exact
/// shape `fast_delete_can_skip_tuple_fetch` admits (every column
/// `ColumnStorageMode::Default`, the PK ART index the table's only index).
fn setup_pk_table() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'one')").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'two')").unwrap();
    db
}

/// `EmbeddedDatabase::begin_transaction` is the writer used throughout the
/// DELETE tests, and the choice is load-bearing: the RAII handle populates
/// NEITHER the global transaction slot (`global_txn_active`) nor the
/// per-session count (`session_txn_count`), which is what every fast
/// UPDATE/DELETE entry point gates on — but it DOES enroll in the
/// uncommitted-write census (`StorageEngine::begin_transaction` →
/// `txn.set_write_census`) and it DOES mutate the shared ART eagerly. So it is
/// a writer the fast DELETE paths can actually observe, which is the whole
/// hazard. A wire session would be gated out before reaching them and would
/// prove nothing.
fn assert_row_two_untouched(db: &EmbeddedDatabase, context: &str) {
    // PROBE by key. The point lookup resolves through the PK ART index, so this
    // asserts the index and the committed rows AGREE about id = 2 — strictly
    // more than a scan says.
    //
    // v4.38.0 had to weaken this to a filter over a full scan because sprinter
    // 8a9b60eeef87 (`begin_transaction()`'s RAII rollback never replayed the
    // ART undo log) left the index without key 2 once the writer below rolled
    // back. That is fixed: the handle now owns its ART undo slot
    // (`EmbeddedDatabase::raii_art_undo`), `Transaction::rollback` replays it and
    // `Transaction::commit` drops it — and, critically for THIS file, the
    // autocommit `db.execute` between the two below can no longer `clear()` the
    // open handle's entries out from under it. So the probe is back.
    //
    // It is equally sound at the call sites that run while the writer is STILL
    // OPEN: the key is genuinely out of the index at that moment, but the
    // uncommitted-index-removal census makes that miss non-authoritative
    // (`index_miss_is_authoritative`, src/sql/executor/scan.rs), so the lookup
    // declines to a filtered scan of committed rows and still finds it.
    let row_two = db.query("SELECT id, v FROM t WHERE id = 2", &[]).unwrap();
    assert_eq!(
        row_two.len(),
        1,
        "{context}: *** WRONG ROW DESTROYED *** the row holding id = 2 is gone (or is no longer \
         reachable by its own key); the statement targeted id = 99, whose index entry an \
         UNCOMMITTED transaction had just moved onto this row"
    );
    assert_eq!(
        row_two[0].values[1],
        Value::String("two".to_string()),
        "{context}: *** WRONG ROW REWRITTEN *** the row holding id = 2 no longer carries the value \
         it was committed with; the statement targeted id = 99"
    );
    // Kept: the key probe cannot see a row destroyed under ANOTHER key, nor a
    // duplicate the statement left behind.
    let found = db.query("SELECT id, v FROM t", &[]).unwrap();
    assert_eq!(found.len(), 2, "{context}: the table lost a row");
}

/// The `v` of the row currently carrying `id`, as a `String`. Panics unless
/// exactly one row matches — a silent zero-row read would make an
/// "unchanged value" assertion vacuous.
fn value_at(db: &EmbeddedDatabase, id: i32) -> String {
    let found = db.query(&format!("SELECT v FROM t WHERE id = {id}"), &[]).unwrap();
    assert_eq!(found.len(), 1, "expected exactly one row with id = {id}");
    match &found[0].values[0] {
        Value::String(s) => s.clone(),
        other => panic!("v is not a string: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// A1 — sprinter 79efe5ebda6e: fast DELETE must not delete the WRONG row
// ---------------------------------------------------------------------------

/// The literal (text-SQL) fast DELETE family.
///
/// `UPDATE t SET id = 99 WHERE id = 2` inside an OPEN transaction runs
/// `on_delete(old)` + `on_insert(new)` against the shared ART at statement time
/// (`execute_in_transaction_inner`'s UPDATE arm), so key 99 already points at
/// the row that still holds id = 2 in committed storage. A concurrent
/// `DELETE FROM t WHERE id = 99` probed that index, got that row id, and — on
/// the `pk_only_delete` shortcut — deleted `data:t:{row_id}` WITHOUT ever
/// reading the row. Nothing in the statement mentioned the row it destroyed,
/// and rolling the other transaction back did not bring it back.
///
/// Under read-committed the answer is 0 rows: no COMMITTED row carries id = 99.
#[test]
fn literal_fast_delete_does_not_delete_a_row_whose_key_an_open_txn_moved() {
    let db = setup_pk_table();
    let tx = db.begin_transaction().unwrap();
    let moved = tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();
    assert_eq!(moved, 1, "the UPDATE must have moved exactly one row");

    // The census sees the removal half of the move — this is the one fact the
    // fast DELETE paths consult, so assert it rather than assume it.
    assert!(
        db.storage.has_uncommitted_index_removals_for_table("t"),
        "an UPDATE that moves an indexed value must arm the removal half of the census"
    );

    let deleted = db.execute("DELETE FROM t WHERE id = 99").unwrap();
    assert_eq!(
        deleted, 0,
        "no COMMITTED row carries id = 99; the only thing that does is an uncommitted index entry"
    );
    assert_row_two_untouched(&db, "literal DELETE, writer still open");

    // ROLLBACK restores the key and must leave the table exactly as it was.
    tx.rollback().unwrap();
    assert!(
        !db.storage.has_uncommitted_index_removals_for_table("t"),
        "ROLLBACK must release the removal half of the census"
    );
    assert_row_two_untouched(&db, "literal DELETE, after the writer rolled back");
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 99"),
        0,
        "the rolled-back move must not leave a row under the new key"
    );
}

/// The parameterised fast DELETE family (`$1`), which has its own spec cache and
/// its own use site and therefore needs its own proof.
#[test]
fn params_fast_delete_does_not_delete_a_row_whose_key_an_open_txn_moved() {
    let db = setup_pk_table();
    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();

    let deleted = db
        .execute_params("DELETE FROM t WHERE id = $1", &[Value::Int4(99)])
        .unwrap();
    assert_eq!(deleted, 0, "params family: no committed row carries id = 99");
    assert_row_two_untouched(&db, "params DELETE, writer still open");

    tx.rollback().unwrap();
    assert_row_two_untouched(&db, "params DELETE, after the writer rolled back");
}

/// The same shape reached through a wire session with no transaction of its own
/// — the route the extended protocol takes (`execute_params_for_session_inner`
/// delegates to `execute_params_inner` when the session holds no transaction).
#[test]
fn session_params_fast_delete_does_not_delete_a_row_whose_key_an_open_txn_moved() {
    let db = setup_pk_table();
    let session = db.create_wire_session("observer").unwrap();
    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();

    let deleted = db
        .execute_params_for_session(session, "DELETE FROM t WHERE id = $1", &[Value::Int4(99)])
        .unwrap();
    assert_eq!(deleted, 0, "session params family: no committed row carries id = 99");
    assert_row_two_untouched(&db, "session params DELETE");

    tx.rollback().unwrap();
    db.destroy_session(session).unwrap();
}

/// NON-VACUITY, both halves. The guard is a DECLINE, not a disable: with no
/// writer open the fast DELETE must still delete, and once the writer is gone
/// the shortcut must come back — otherwise the two tests above would pass on a
/// build where DELETE simply stopped working.
#[test]
fn fast_delete_still_deletes_when_no_transaction_has_moved_a_key() {
    let db = setup_pk_table();

    // Nothing staged anywhere: the ordinary fast path, literal and params.
    assert!(!db.storage.has_uncommitted_index_removals_for_table("t"));
    assert_eq!(db.execute("DELETE FROM t WHERE id = 1").unwrap(), 1);
    assert_eq!(
        db.execute_params("DELETE FROM t WHERE id = $1", &[Value::Int4(2)])
            .unwrap(),
        1
    );
    assert_eq!(rows(&db, "SELECT id FROM t"), 0);

    // And a COMMITTED move is the ordinary case again: the row really does
    // carry id = 99 now, so the DELETE must find and remove it.
    db.execute("INSERT INTO t (id, v) VALUES (2, 'two')").unwrap();
    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();
    tx.commit().unwrap();
    assert!(
        !db.storage.has_uncommitted_index_removals_for_table("t"),
        "COMMIT must release the census, restoring the shortcut"
    );
    assert_eq!(
        db.execute("DELETE FROM t WHERE id = 99").unwrap(),
        1,
        "after COMMIT the row genuinely carries id = 99 and must be deleted"
    );
    assert_eq!(rows(&db, "SELECT id FROM t"), 0);
}

/// A staged move on ANOTHER table must not cost this one its fast path, and
/// must not change its answers. Pins the per-table half of the census gate.
#[test]
fn a_moved_key_on_another_table_neither_gates_nor_changes_this_one() {
    let db = setup_pk_table();
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO u (id, v) VALUES (5, 'five')").unwrap();

    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE u SET id = 77 WHERE id = 5").unwrap();
    assert!(db.storage.has_uncommitted_index_removals_for_table("u"));
    assert!(
        !db.storage.has_uncommitted_index_removals_for_table("t"),
        "a writer on `u` says nothing about `t`"
    );

    assert_eq!(db.execute("DELETE FROM t WHERE id = 1").unwrap(), 1);
    assert_eq!(rows(&db, "SELECT id FROM t"), 1);
    tx.rollback().unwrap();
}

/// The UPDATE half of the same defect. `try_fast_update` and
/// `try_execute_fast_update_param_spec` resolve the row they are about to
/// OVERWRITE through the same `get_row_by_typed_pk_for_write_with_schema` →
/// `get_row_by_pk_inner` → `pk_index_lookup` chain the DELETE paths used, with
/// no census gate and no check that the row carries the probed key. So
/// `UPDATE t SET v = … WHERE id = 99` rewrote the row that still holds id = 2.
///
/// Destroying the old value is the same data loss as deleting the row, and it
/// survives the other transaction's ROLLBACK the same way.
#[test]
fn literal_fast_update_does_not_rewrite_a_row_whose_key_an_open_txn_moved() {
    let db = setup_pk_table();
    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();
    assert!(db.storage.has_uncommitted_index_removals_for_table("t"));

    let updated = db.execute("UPDATE t SET v = 'clobbered' WHERE id = 99").unwrap();
    assert_eq!(
        updated, 0,
        "no COMMITTED row carries id = 99; the only thing that does is an uncommitted index entry"
    );
    assert_row_two_untouched(&db, "literal UPDATE, writer still open");

    tx.rollback().unwrap();
    assert_row_two_untouched(&db, "literal UPDATE, after the writer rolled back");
}

/// The parameterised UPDATE family, which has its own spec cache and its own
/// use site.
#[test]
fn params_fast_update_does_not_rewrite_a_row_whose_key_an_open_txn_moved() {
    let db = setup_pk_table();
    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();

    let updated = db
        .execute_params(
            "UPDATE t SET v = $1 WHERE id = $2",
            &[Value::String("clobbered".to_string()), Value::Int4(99)],
        )
        .unwrap();
    assert_eq!(updated, 0, "params family: no committed row carries id = 99");
    assert_row_two_untouched(&db, "params UPDATE, writer still open");

    tx.rollback().unwrap();
    assert_row_two_untouched(&db, "params UPDATE, after the writer rolled back");
}

/// The same shape through a wire session with no transaction of its own — the
/// extended protocol's autocommit route.
#[test]
fn session_params_fast_update_does_not_rewrite_a_row_whose_key_an_open_txn_moved() {
    let db = setup_pk_table();
    let session = db.create_wire_session("observer").unwrap();
    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();

    let updated = db
        .execute_params_for_session(
            session,
            "UPDATE t SET v = $1 WHERE id = $2",
            &[Value::String("clobbered".to_string()), Value::Int4(99)],
        )
        .unwrap();
    assert_eq!(updated, 0, "session params family: no committed row carries id = 99");
    assert_row_two_untouched(&db, "session params UPDATE");

    tx.rollback().unwrap();
    db.destroy_session(session).unwrap();
}

/// NON-VACUITY for the UPDATE guard, both halves — this is what separates a
/// DECLINE from a disabled fast path. With nothing staged the fast UPDATE must
/// still run and must still hit the RIGHT row (checked by reading the value
/// back under both the updated and the untouched key), and once a move has
/// COMMITTED the row really does carry the new key and must be updated under it.
#[test]
fn fast_update_still_updates_the_right_row_when_no_transaction_has_moved_a_key() {
    let db = setup_pk_table();
    assert!(!db.storage.has_uncommitted_index_removals_for_table("t"));

    assert_eq!(db.execute("UPDATE t SET v = 'ONE' WHERE id = 1").unwrap(), 1);
    assert_eq!(value_at(&db, 1), "ONE");
    assert_eq!(
        value_at(&db, 2),
        "two",
        "the literal UPDATE touched a row it did not name"
    );

    assert_eq!(
        db.execute_params(
            "UPDATE t SET v = $1 WHERE id = $2",
            &[Value::String("TWO".to_string()), Value::Int4(2)],
        )
        .unwrap(),
        1
    );
    assert_eq!(value_at(&db, 2), "TWO");
    assert_eq!(
        value_at(&db, 1),
        "ONE",
        "the params UPDATE touched a row it did not name"
    );

    // A COMMITTED move is the ordinary case again.
    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();
    tx.commit().unwrap();
    assert!(!db.storage.has_uncommitted_index_removals_for_table("t"));
    assert_eq!(
        db.execute("UPDATE t SET v = 'MOVED' WHERE id = 99").unwrap(),
        1,
        "after COMMIT the row genuinely carries id = 99 and must be updated"
    );
    assert_eq!(value_at(&db, 99), "MOVED");
    assert_eq!(value_at(&db, 1), "ONE");
}

// ---------------------------------------------------------------------------
// A2 — sprinter 6780488554df: params multi-row INSERT is all-or-nothing
// ---------------------------------------------------------------------------

fn setup_insert_table() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ins (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO ins (id, v) VALUES (3, 'incumbent')").unwrap();
    db
}

const MULTI_ROW_INSERT: &str = "INSERT INTO ins (id, v) VALUES ($1, $2), ($3, $4), ($5, $6)";

fn three_rows(third_id: i32) -> Vec<Value> {
    vec![
        Value::Int4(1),
        Value::String("a".to_string()),
        Value::Int4(2),
        Value::String("b".to_string()),
        Value::Int4(third_id),
        Value::String("c".to_string()),
    ]
}

/// The defect: the Insert arm of `execute_plan_with_params_inner` resolved no
/// active transaction on the autocommit path and so wrote each row with
/// `insert_tuple_branch_aware_with_schema` — a direct, already-durable storage
/// write. Row 3 collides with the incumbent id = 3, the arm returns the 23505 …
/// and rows 1 and 2 are already committed. PostgreSQL persists nothing.
#[test]
fn params_multirow_insert_persists_nothing_when_a_later_row_violates_unique() {
    let db = setup_insert_table();
    let before = rows(&db, "SELECT id FROM ins");
    assert_eq!(before, 1);

    let err = db
        .execute_params(MULTI_ROW_INSERT, &three_rows(3))
        .expect_err("row 3 duplicates the incumbent primary key and the statement must fail");
    assert_unique_violation(&err, "params multi-row INSERT");

    assert_eq!(
        rows(&db, "SELECT id FROM ins"),
        before,
        "*** PARTIAL INSERT *** rows 1..N-1 of a failed multi-row INSERT were left committed"
    );
    assert_eq!(
        rows(&db, "SELECT id FROM ins WHERE id = 1"),
        0,
        "row 1 must not survive"
    );
    assert_eq!(
        rows(&db, "SELECT id FROM ins WHERE id = 2"),
        0,
        "row 2 must not survive"
    );

    // The rollback has to undo the eager ART entries as well as the rows: the
    // staging path inserts every key into the index at statement time, so if the
    // undo log were not replayed the retry below would fail with a duplicate key
    // against rows that do not exist.
    assert_eq!(
        db.execute_params(
            MULTI_ROW_INSERT,
            &[
                Value::Int4(1),
                Value::String("a".to_string()),
                Value::Int4(2),
                Value::String("b".to_string()),
                Value::Int4(5),
                Value::String("c".to_string()),
            ],
        )
        .expect("the retry must not collide with the rolled-back rows' index entries"),
        3
    );
    assert_eq!(rows(&db, "SELECT id FROM ins"), 4);
}

/// The same statement over the route the PostgreSQL extended protocol takes for
/// an autocommit Execute (a wire session holding no transaction of its own).
#[test]
fn session_params_multirow_insert_persists_nothing_on_a_duplicate() {
    let db = setup_insert_table();
    let session = db.create_wire_session("prisma").unwrap();

    let err = db
        .execute_params_for_session(session, MULTI_ROW_INSERT, &three_rows(3))
        .expect_err("row 3 duplicates the incumbent primary key");
    assert_unique_violation(&err, "extended-protocol multi-row INSERT");

    assert_eq!(
        rows(&db, "SELECT id FROM ins"),
        1,
        "*** PARTIAL INSERT *** over the extended protocol"
    );
    db.destroy_session(session).unwrap();
}

/// A duplicate WITHIN the batch (rows 2 and 3 carry the same key) must behave
/// the same way — the collision is found against row 2's own eagerly-inserted
/// ART entry rather than against a committed row.
#[test]
fn params_multirow_insert_persists_nothing_when_the_batch_duplicates_itself() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ins (id INTEGER PRIMARY KEY, v TEXT)").unwrap();

    let err = db
        .execute_params(MULTI_ROW_INSERT, &three_rows(2))
        .expect_err("rows 2 and 3 carry the same primary key");
    assert_unique_violation(&err, "self-duplicating multi-row INSERT");
    assert_eq!(
        rows(&db, "SELECT id FROM ins"),
        0,
        "*** PARTIAL INSERT *** a self-duplicating batch left its earlier rows behind"
    );
}

/// NON-VACUITY: the wrap must not turn a perfectly good multi-row INSERT into a
/// no-op, and the rows must be readable — through the table scan AND through
/// the PK index, so the implicit transaction's commit is proved to have landed
/// both the `data:` rows and their index entries.
#[test]
fn params_multirow_insert_commits_every_row_when_none_conflict() {
    let db = setup_insert_table();
    let count = db.execute_params(MULTI_ROW_INSERT, &three_rows(4)).unwrap();
    assert_eq!(count, 3, "all three rows must be inserted");
    assert_eq!(rows(&db, "SELECT id FROM ins"), 4);
    for id in [1, 2, 4] {
        assert_eq!(
            rows(&db, &format!("SELECT id FROM ins WHERE id = {id}")),
            1,
            "row {id} must be reachable through the primary key"
        );
    }
}

/// The single-row params INSERT fast path is deliberately NOT wrapped. It is
/// atomic by construction and it is the throughput-critical shape; this pins
/// that it still behaves, both for the good row and for the duplicate.
#[test]
fn params_single_row_insert_is_unchanged() {
    let db = setup_insert_table();
    assert_eq!(
        db.execute_params(
            "INSERT INTO ins (id, v) VALUES ($1, $2)",
            &[Value::Int4(9), Value::String("nine".to_string())]
        )
        .unwrap(),
        1
    );
    let err = db
        .execute_params(
            "INSERT INTO ins (id, v) VALUES ($1, $2)",
            &[Value::Int4(9), Value::String("again".to_string())],
        )
        .expect_err("a single-row duplicate must still raise");
    assert_unique_violation(&err, "single-row params INSERT");
    assert_eq!(rows(&db, "SELECT id FROM ins"), 2);
}

/// Inside an EXPLICIT transaction the statement is already part of a larger
/// unit, and the gate must leave it alone: the failure rolls the whole
/// transaction back, exactly as before.
#[test]
fn params_multirow_insert_inside_an_explicit_transaction_still_rolls_back_whole() {
    let db = setup_insert_table();
    let session = db.create_wire_session("txn").unwrap();
    db.execute_for_session(session, "BEGIN").unwrap();
    let err = db
        .execute_params_for_session(session, MULTI_ROW_INSERT, &three_rows(3))
        .expect_err("row 3 duplicates the incumbent primary key");
    assert_unique_violation(&err, "multi-row INSERT in an explicit transaction");
    db.execute_for_session(session, "ROLLBACK").unwrap();
    assert_eq!(rows(&db, "SELECT id FROM ins"), 1);
    db.destroy_session(session).unwrap();
}

// ---------------------------------------------------------------------------
// A3 — sprinter 3f8e05a39baf: a PK index must not evict a same-named index
// ---------------------------------------------------------------------------

/// `{table}_pkey` is DERIVED, and `indexes` is ONE map for the whole database,
/// so the name is not the engine's to assume free: `CREATE INDEX t_pkey ON
/// other (x)` is a legal statement that claims it before table `t` exists.
///
/// `create_pk_index` checked only the per-table `pk_indexes` map and then
/// inserted into `indexes` unconditionally, so creating `t` EVICTED the entry
/// belonging to `other`. Two things then go wrong, and this test observes both:
///
/// 1. `other`'s index is claimed but gone — `find_column_index("other", "x")`
///    filters on `entry.table`, and the entry now says `t`.
/// 2. Worse, `table_indexes["other"]` still LISTS `t_pkey`, and `on_insert`
///    resolves a table's indexes from that list without re-checking the owner.
///    So every INSERT into `other` was maintained in `t`'s PRIMARY KEY tree —
///    and the first insert into `t` under a key `other` happened to use failed
///    with a duplicate-key error against a row in a different table.
#[test]
fn a_pk_index_does_not_evict_a_user_index_that_already_holds_its_name() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE other (id INTEGER, x INTEGER)").unwrap();
    db.execute("CREATE INDEX t_pkey ON other (x)").unwrap();
    assert_eq!(
        db.storage.art_indexes().find_column_index("other", "x").as_deref(),
        Some("t_pkey"),
        "setup: the user index must be registered on `other` before `t` exists"
    );

    // The collision.
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();

    assert!(
        db.storage.art_indexes().find_column_index("other", "x").is_some(),
        "*** INDEX EVICTED *** creating table `t` took the registry entry belonging to \
         `other`'s index, which is still named by `other`'s side maps"
    );
    assert!(
        db.storage.art_indexes().find_column_index("t", "id").is_some(),
        "and table `t` must still have an enforcing PRIMARY KEY index of its own"
    );

    // The user-visible consequence of the eviction: `other`'s rows were being
    // maintained in `t`'s PRIMARY KEY tree.
    db.execute("INSERT INTO other (id, x) VALUES (42, 7)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (42, 'real')")
        .expect("*** FALSE DUPLICATE *** `t` is empty; the key 42 in its PK tree came from `other`");

    // Both tables still answer correctly, through their own indexes.
    assert_eq!(rows(&db, "SELECT id FROM t WHERE id = 42"), 1);
    assert_eq!(rows(&db, "SELECT id FROM other WHERE x = 7"), 1);
    assert_eq!(rows(&db, "SELECT id FROM other WHERE x = 999"), 0);

    // And `t`'s PRIMARY KEY is genuinely enforcing under whatever name it got.
    let err = db
        .execute("INSERT INTO t (id, v) VALUES (42, 'dup')")
        .expect_err("a real duplicate must still be refused");
    assert_unique_violation(&err, "PRIMARY KEY under a minted name");
    assert_eq!(rows(&db, "SELECT id FROM t"), 1);
}
