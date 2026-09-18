//! Storage/PK batch G1 — two defects that both end the same way: the PK ART
//! index and the committed rows disagree, silently, for the life of the
//! process.
//!
//! * **sprinter f32ba64c00a7** — the SERIAL / IDENTITY auto-fill blocks gated on
//!   `col.primary_key` ALONE and then assigned `Value::Int8(row_id)` for ANY
//!   declared column type, so `CREATE TABLE t (id TEXT, v INT, PRIMARY KEY (id))`
//!   + `INSERT INTO t (v) VALUES (1)` wrote an INTEGER into a TEXT primary key.
//!   Three consequences, all reproduced below on the pre-fix tree (v4.37.0
//!   binary, `heliosdb-nano repl -m`):
//!
//!   ```text
//!   CREATE TABLE t2 (id TEXT, v INT, PRIMARY KEY (id));   -- Query OK
//!   INSERT INTO t2 (v) VALUES (1);                        -- Query OK, 1 row
//!   SELECT * FROM t2;                                     -- 1 row: id = 1
//!   SELECT * FROM t2 WHERE id = '1';                      -- 0 rows  <<<<
//!   SELECT length(id) FROM t2;                            -- ERROR: LENGTH
//!                                                         --   requires a
//!                                                         --   string argument
//!   ```
//!
//!   i.e. the row is unfindable by point lookup while a full scan returns it
//!   (the ART index encoded the key at the INTEGER's type width; every later
//!   probe built from the declared TEXT type encodes differently — the same
//!   class as the v3.60.6 DECIMAL-PK defect), and the stored row disagrees with
//!   its own declared schema. PostgreSQL answers a NULL / omitted primary key
//!   with no DEFAULT and no identity with `23502`, never by inventing a key of
//!   another type.
//!
//! * **sprinter 8a9b60eeef87** — `EmbeddedDatabase::begin_transaction()`'s RAII
//!   handle routes its eager ART undo entries into the shared `art_undo_log`
//!   (`push_art_undo` keys on `txn.session_id()`, and the handle has none), and
//!   `Transaction::rollback` was a bare `self.tx.rollback()`: it discarded the
//!   write set and left the index mutations standing. After a rolled-back
//!   `UPDATE t SET id = 99 WHERE id = 2` the committed row still carried
//!   `id = 2` while the PK index did not hold key 2 at all. `Transaction::commit`
//!   had the mirror-image hole: it never CLEARED the log, so the next rollback
//!   to drain it would replay a committed transaction's ops. And because the
//!   handle sets neither `global_txn_active` nor the per-session count,
//!   autocommit statements run alongside an open one — and every autocommit path
//!   ends with `art_undo_log.write().clear()`, which discarded the open handle's
//!   pending entries outright. The fix gives each handle its own slot
//!   (`raii_art_undo`), the session-less twin of `session_art_undo`.
//!
//! # Both executor families
//!
//! Every write assertion here is made on the text family (`execute`) AND the
//! bound-params family (`execute_params` / `execute_params_for_session`, which
//! is also what the PostgreSQL extended protocol reaches). They are separate
//! implementations with separate INSERT arms; a fix that lands on one only is
//! how this repo has repeatedly shipped half a fix.
//!
//! # SQLSTATE
//!
//! The wire classifiers are not reachable from an integration test
//! (`sqlstate_for_error` is `pub(crate)`), so — as in `tests/ddl_validation_batch_a.rs`
//! and `tests/gh_issue_27*.rs` — these tests pin the MESSAGE SHAPE the
//! classifiers key on, through the very const both of them anchor on:
//! `NOT_NULL_VIOLATION_MARKER` → `23502 not_null_violation` on the PostgreSQL
//! wire, `1048 ER_BAD_NULL_ERROR` on the MySQL wire.

use heliosdb_nano::{EmbeddedDatabase, Value, NOT_NULL_VIOLATION_MARKER};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn rows(db: &EmbeddedDatabase, sql: &str) -> usize {
    db.query(sql, &[]).unwrap().len()
}

/// The refusal a NULL in a non-integer PRIMARY KEY must produce: PostgreSQL's
/// own `null value in column "id" of relation "t" violates not-null constraint`,
/// naming the column, and carrying the marker the two wire classifiers key
/// `23502` / ER_BAD_NULL_ERROR on.
fn assert_null_pk_refused(err: &heliosdb_nano::Error, column: &str, context: &str) {
    let text = err.to_string();
    assert!(
        text.contains(NOT_NULL_VIOLATION_MARKER),
        "{context}: the refusal must carry the marker the wire classifiers map to 23502 \
         not_null_violation / 1048 ER_BAD_NULL_ERROR, got: {err}"
    );
    assert!(
        text.contains(column),
        "{context}: the refusal must name the offending column '{column}', got: {err}"
    );
}

/// A single-column TEXT primary key declared at TABLE level.
///
/// The table-level spelling is load-bearing and is why the defect survived
/// #108: `sql_column_def_to_column_def` sets `not_null` for an INLINE
/// `id TEXT PRIMARY KEY`, so the executor's NOT NULL gate rejects the NULL
/// before any fill site sees it, but the table-level `PRIMARY KEY (id)` loop
/// (`src/sql/planner.rs`) marks `primary_key` and deliberately leaves
/// `not_null` alone — so the NULL reaches the storage auto-fill. Both spellings
/// are pinned below; only this one exercised the bug.
fn text_pk_table() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id TEXT, v INT, PRIMARY KEY (id))").unwrap();
    db
}

/// Two committed rows under a single-column INTEGER primary key — the shape of
/// the coordinator's 8a9b60eeef87 probe.
fn int_pk_table() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'one')").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'two')").unwrap();
    db
}

/// Every row a full scan returns must also be reachable by its OWN key.
///
/// This is the invariant both items break from opposite directions: f32ba64c00a7
/// by storing a key of the wrong type, 8a9b60eeef87 by leaving the index without
/// a key the row still carries.
fn assert_index_agrees_with_scan(db: &EmbeddedDatabase, context: &str) {
    let all = db.query("SELECT id FROM t", &[]).unwrap();
    for row in &all {
        let id = match row.values.first() {
            Some(Value::Int4(n)) => i64::from(*n),
            Some(Value::Int8(n)) => *n,
            other => panic!("{context}: unexpected id value {other:?}"),
        };
        assert_eq!(
            rows(db, &format!("SELECT id FROM t WHERE id = {id}")),
            1,
            "{context}: the scan returns a row with id = {id} that a point lookup on its own key \
             cannot find — the PK index and the committed rows disagree"
        );
    }
}

// ---------------------------------------------------------------------------
// f32ba64c00a7 — a NULL PK of a non-integer declared type is 23502, never an
// invented integer
// ---------------------------------------------------------------------------

/// The text family, omitted primary key — the reported shape.
#[test]
fn text_pk_omitted_is_refused_by_the_text_family() {
    let db = text_pk_table();
    let err = db.execute("INSERT INTO t (v) VALUES (1)").unwrap_err();
    assert_null_pk_refused(&err, "id", "text family, omitted TEXT PK");
    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "the refused INSERT must persist nothing; it used to store Value::Int8(row_id) in a \
         column declared TEXT"
    );
}

/// The text family, explicit NULL. Same rule, different arm of the INSERT
/// value path (a supplied NULL rather than an unsupplied column).
#[test]
fn text_pk_explicit_null_is_refused_by_the_text_family() {
    let db = text_pk_table();
    let err = db.execute("INSERT INTO t VALUES (NULL, 1)").unwrap_err();
    assert_null_pk_refused(&err, "id", "text family, explicit NULL TEXT PK");
    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "the refused INSERT must persist nothing"
    );
}

/// The bound-params family — `execute_params`, which reaches
/// `parameterized_plan_cached` (no optimizer passes) and is also the path the
/// PostgreSQL extended protocol takes. Its INSERT arm has its own fill site.
#[test]
fn text_pk_omitted_is_refused_by_the_params_family() {
    let db = text_pk_table();
    let err = db
        .execute_params("INSERT INTO t (v) VALUES ($1)", &[Value::Int4(1)])
        .unwrap_err();
    assert_null_pk_refused(&err, "id", "params family, omitted TEXT PK");
    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "the refused INSERT must persist nothing"
    );

    // Literal NULL with a bound value beside it: still the params family, without
    // making the assertion depend on how a NULL-typed PARAMETER is inferred.
    let err = db
        .execute_params("INSERT INTO t (id, v) VALUES (NULL, $1)", &[Value::Int4(1)])
        .unwrap_err();
    assert_null_pk_refused(&err, "id", "params family, explicit NULL TEXT PK");
    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "the refused INSERT must persist nothing"
    );
}

/// The wire-session route — `execute_params_for_session` with no transaction of
/// its own, which is the extended protocol's autocommit shape.
#[test]
fn text_pk_omitted_is_refused_on_the_session_route() {
    let db = text_pk_table();
    let session = db.create_wire_session("writer").unwrap();
    let err = db
        .execute_params_for_session(session, "INSERT INTO t (v) VALUES ($1)", &[Value::Int4(1)])
        .unwrap_err();
    assert_null_pk_refused(&err, "id", "session params family, omitted TEXT PK");
    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "the refused INSERT must persist nothing"
    );
    db.destroy_session(session).unwrap();
}

/// The multi-row INSERT, a different arm again. Pre-fix it stored THREE rows
/// keyed 1, 2, 3 in a TEXT primary key (observed on the v4.37.0 binary), so
/// whichever funnel serves it — the batch path reaches
/// `prepare_tuple_for_transaction_insert`, which had to become fallible for the
/// refusal to be able to leave it — the shape must be refused.
#[test]
fn text_pk_omitted_is_refused_by_the_batch_insert_funnel() {
    let db = text_pk_table();
    let err = db.execute("INSERT INTO t (v) VALUES (1), (2), (3)").unwrap_err();
    assert_null_pk_refused(&err, "id", "batch INSERT funnel, omitted TEXT PK");
    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "the refused multi-row INSERT must persist nothing; it used to store three rows keyed \
         1, 2, 3 in a TEXT primary key"
    );
}

/// The same INSERT inside the RAII transaction handle — the in-transaction
/// INSERT arm, a fourth fill site.
#[test]
fn text_pk_omitted_is_refused_inside_a_transaction() {
    let db = text_pk_table();
    let tx = db.begin_transaction().unwrap();
    let err = tx.execute("INSERT INTO t (v) VALUES (1)").unwrap_err();
    assert_null_pk_refused(&err, "id", "in-transaction INSERT, omitted TEXT PK");
    tx.rollback().unwrap();
    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "the refused INSERT must persist nothing"
    );
}

/// ANY declared type, not just TEXT — the item's wording. UUID takes the same
/// `_ =>` arm the old code funnelled every non-`Int2`/`Int4` type into.
#[test]
fn uuid_pk_omitted_is_refused_on_both_families() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id UUID, v INT, PRIMARY KEY (id))").unwrap();

    let err = db.execute("INSERT INTO t (v) VALUES (1)").unwrap_err();
    assert_null_pk_refused(&err, "id", "text family, omitted UUID PK");

    let err = db
        .execute_params("INSERT INTO t (v) VALUES ($1)", &[Value::Int4(1)])
        .unwrap_err();
    assert_null_pk_refused(&err, "id", "params family, omitted UUID PK");

    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        0,
        "a UUID primary key used to be filled with Value::Int8(row_id) too"
    );
}

/// The INLINE spelling. Already refused before this item (the planner sets
/// `not_null` for an inline `PRIMARY KEY`, so #108's NOT NULL gate catches it
/// in the executor), which is exactly why two earlier passes called the defect
/// "not reproducible" — pinned so the two spellings cannot drift apart again.
#[test]
fn inline_text_pk_omitted_is_refused_too() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id TEXT PRIMARY KEY, v INT)").unwrap();
    assert!(
        db.execute("INSERT INTO t (v) VALUES (1)").is_err(),
        "an omitted inline TEXT PRIMARY KEY must be refused"
    );
    assert!(
        db.execute("INSERT INTO t VALUES (NULL, 1)").is_err(),
        "an explicit NULL into an inline TEXT PRIMARY KEY must be refused"
    );
    assert_eq!(rows(&db, "SELECT id FROM t"), 0, "neither INSERT may persist anything");
}

/// THE user-visible symptom, as a probe.
///
/// Pre-fix the bad INSERT succeeded and left a table in which a full scan
/// returned a row that no point lookup could find, and whose `id` was not a
/// string at all. Post-fix the INSERT is refused and the two readings agree at
/// zero; a legitimately supplied TEXT key is then found by BOTH, and behaves
/// like the TEXT it was declared as.
#[test]
fn text_pk_point_lookup_and_full_scan_agree() {
    let db = text_pk_table();

    assert!(
        db.execute("INSERT INTO t (v) VALUES (1)").is_err(),
        "the INSERT that produced the divergence must be refused"
    );
    assert_eq!(rows(&db, "SELECT id FROM t"), 0, "full scan: nothing was stored");
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = '1'"),
        0,
        "point lookup: nothing was stored"
    );

    // Non-vacuity: with a real TEXT key supplied, both readings find it.
    db.execute("INSERT INTO t (id, v) VALUES ('1', 1)").unwrap();
    assert_eq!(rows(&db, "SELECT id FROM t"), 1, "full scan finds the legitimate row");
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = '1'"),
        1,
        "the point lookup must find the row the scan returns — the PK index encodes the key at \
         the declared TEXT width, which an invented Int8 key did not"
    );

    // And the stored row agrees with its own declared schema. Pre-fix this
    // query failed with "LENGTH requires a string argument" because the value
    // in a column declared TEXT was an integer.
    let lengths = db.query("SELECT length(id) FROM t", &[]).unwrap();
    assert_eq!(lengths.len(), 1, "length() over a TEXT primary key must work");
}

/// NON-VACUITY for the fix itself: legitimate integer auto-fill is untouched.
///
/// Without this, a build that simply stopped filling NULL primary keys would
/// pass every test above. `SERIAL` / `BIGSERIAL` / `SMALLSERIAL` expand to
/// `int4` / `int8` / `int2`, which is precisely the family the gate admits.
#[test]
fn integer_serial_pk_autofill_still_works_on_both_families() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, v INT)").unwrap();

    // Text family.
    assert_eq!(db.execute("INSERT INTO t (v) VALUES (7)").unwrap(), 1);
    // Params family.
    assert_eq!(
        db.execute_params("INSERT INTO t (v) VALUES ($1)", &[Value::Int4(8)])
            .unwrap(),
        1
    );
    // Batch funnel.
    assert_eq!(db.execute("INSERT INTO t (v) VALUES (9), (10)").unwrap(), 2);

    let all = db.query("SELECT id, v FROM t", &[]).unwrap();
    assert_eq!(all.len(), 4, "every auto-filled INSERT must have landed");
    for row in &all {
        match row.values.first() {
            Some(Value::Int4(n)) => assert!(*n > 0, "SERIAL must generate a positive key, got {n}"),
            Some(Value::Int8(n)) => assert!(*n > 0, "SERIAL must generate a positive key, got {n}"),
            other => panic!("a SERIAL primary key must be an integer, got {other:?}"),
        }
    }
    // The generated keys are reachable through the index, not just the scan.
    assert_index_agrees_with_scan(&db, "SERIAL auto-fill");
}

/// The SQL-standard spelling of the same thing. `GENERATED … AS IDENTITY` on an
/// integer column must keep auto-filling.
#[test]
fn integer_identity_pk_autofill_still_works() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v INT)")
        .unwrap();
    assert_eq!(db.execute("INSERT INTO t (v) VALUES (1)").unwrap(), 1);
    assert_eq!(rows(&db, "SELECT id FROM t"), 1, "IDENTITY must still auto-fill");
    assert_index_agrees_with_scan(&db, "IDENTITY auto-fill");
}

/// A plain INTEGER primary key declared at TABLE level keeps its pre-existing
/// auto-fill behaviour. Not what this item is about (#108 settled the
/// integer-PK question through `nullable`), pinned so the type gate is not
/// mistaken for a behaviour change on the integer family.
#[test]
fn integer_table_level_pk_autofill_is_unchanged() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER, v INT, PRIMARY KEY (id))")
        .unwrap();
    assert_eq!(db.execute("INSERT INTO t (v) VALUES (1)").unwrap(), 1);
    assert_eq!(rows(&db, "SELECT id FROM t"), 1);
    assert_index_agrees_with_scan(&db, "integer table-level PK auto-fill");
}

// ---------------------------------------------------------------------------
// 8a9b60eeef87 — the RAII handle's ROLLBACK must replay the ART undo log, and
// its COMMIT must clear it
// ---------------------------------------------------------------------------

/// The coordinator's probe, verbatim: no DELETE, no fast path.
///
/// Pre-fix `SELECT id FROM t` returned both rows (the data rolled back
/// correctly) while `SELECT id, v FROM t WHERE id = 2` returned NOTHING — the
/// PK index never got key 2 back, so every point lookup, FK probe, fast
/// UPDATE/DELETE and index-driven plan missed a row that exists.
#[test]
fn raii_rollback_of_a_pk_moving_update_restores_the_original_key() {
    let db = int_pk_table();

    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();
    tx.rollback().unwrap();

    assert_eq!(
        rows(&db, "SELECT id FROM t"),
        2,
        "the data must roll back (it always did)"
    );
    let row_two = db.query("SELECT id, v FROM t WHERE id = 2", &[]).unwrap();
    assert_eq!(
        row_two.len(),
        1,
        "*** INDEX LOST A LIVE KEY *** the committed row carries id = 2 but the PK index does \
         not hold key 2: the RAII rollback did not replay the ART undo log"
    );
    assert_eq!(row_two[0].values[1], Value::String("two".to_string()));
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 99"),
        0,
        "the abandoned key must be gone from the index"
    );
    assert_index_agrees_with_scan(&db, "after a rolled-back PK-moving UPDATE");
}

/// The reverse pin. A rollback that replayed too much would look identical to a
/// correct one on the test above; this fails unless COMMIT keeps the move.
#[test]
fn raii_commit_of_a_pk_moving_update_keeps_the_new_key() {
    let db = int_pk_table();

    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();
    tx.commit().unwrap();

    assert_eq!(rows(&db, "SELECT id FROM t"), 2, "COMMIT keeps both rows");
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 99"),
        1,
        "the committed row must be reachable under its new key"
    );
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 2"),
        0,
        "the old key must not survive the commit"
    );
    assert_index_agrees_with_scan(&db, "after a committed PK-moving UPDATE");
}

/// The second half of the fix, and the part that would be a WORSE bug if it
/// were missed: a COMMITTED transaction's undo entries must be DROPPED.
///
/// `Transaction::commit` never cleared them, so they stayed in the shared
/// `art_undo_log`. The moment `rollback` started draining that log (the first
/// half of this fix), the next rollback to run — here an unrelated second
/// transaction's — would have replayed the FIRST transaction's ops and stripped
/// key 99 from a row that is committed at id = 99.
///
/// The shipped fix makes that structurally impossible (each handle owns its own
/// slot, and `commit` drops it), so this is a guard against a refactor back to a
/// shared log rather than a reproduction — the reproduction is
/// `an_autocommit_statement_does_not_discard_an_open_raii_transactions_undo`
/// below, which is the shape that actually bites.
#[test]
fn raii_commit_drops_its_undo_so_a_later_rollback_cannot_replay_it() {
    let db = int_pk_table();

    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();
    tx.commit().unwrap();

    // Read through a SCAN here, deliberately: the probe below must be the
    // first point lookup on key 99, so nothing it asserts can be answered from
    // a cached result produced before the second transaction ran.
    let ids = db.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(ids.len(), 2, "the commit kept both rows");

    // An unrelated second transaction that rolls back.
    let tx2 = db.begin_transaction().unwrap();
    tx2.execute("INSERT INTO t (id, v) VALUES (3, 'three')").unwrap();
    tx2.rollback().unwrap();

    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 99"),
        1,
        "*** A COMMITTED TRANSACTION'S UNDO WAS REPLAYED *** the second transaction's ROLLBACK \
         drained undo entries the first transaction's COMMIT should have discarded"
    );
    assert_eq!(rows(&db, "SELECT id FROM t"), 2, "the rolled-back INSERT must be gone");
    assert_index_agrees_with_scan(&db, "after commit-then-rollback");
}

/// A rolled-back DELETE brings the key back.
#[test]
fn raii_rollback_of_a_delete_restores_the_key() {
    let db = int_pk_table();

    let tx = db.begin_transaction().unwrap();
    tx.execute("DELETE FROM t WHERE id = 2").unwrap();
    tx.rollback().unwrap();

    assert_eq!(rows(&db, "SELECT id FROM t"), 2, "the row must come back");
    assert_eq!(
        rows(&db, "SELECT id, v FROM t WHERE id = 2"),
        1,
        "*** INDEX LOST A LIVE KEY *** the rolled-back DELETE left the PK index without key 2"
    );
    assert_index_agrees_with_scan(&db, "after a rolled-back DELETE");
}

/// A rolled-back INSERT must not leave a key behind.
///
/// The row-count assertion alone cannot tell a clean index from a stale entry,
/// so the real pin is the re-INSERT: a leftover ART entry for key 3 would make
/// the second INSERT collide with a row that does not exist.
#[test]
fn raii_rollback_of_an_insert_leaves_no_key() {
    let db = int_pk_table();

    let tx = db.begin_transaction().unwrap();
    tx.execute("INSERT INTO t (id, v) VALUES (3, 'three')").unwrap();
    tx.rollback().unwrap();

    assert_eq!(rows(&db, "SELECT id FROM t"), 2, "the rolled-back row must be gone");

    db.execute("INSERT INTO t (id, v) VALUES (3, 'really three')")
        .unwrap_or_else(|e| panic!("a rolled-back INSERT left key 3 in the PK index; re-inserting it failed: {e}"));
    assert_eq!(rows(&db, "SELECT id FROM t"), 3);
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 3"),
        1,
        "the re-inserted row must be reachable by its key"
    );
    assert_index_agrees_with_scan(&db, "after a rolled-back then re-run INSERT");
}

/// The interleaving that forced per-handle undo slots.
///
/// The RAII handle populates NEITHER `global_txn_active` NOR the per-session
/// transaction count, so ordinary autocommit statements keep running while one
/// is open — and every autocommit path ends its implicit transaction with
/// `self.art_undo_log.write().clear()`. With the handle's entries in that shared
/// log, the `INSERT` below silently threw away the pending undo for the `UPDATE`
/// above it, and the `rollback` then had nothing to replay: the same lost key as
/// the plain probe, reachable without any fast path. This is exactly the shape
/// `tests/storage_dml_batch_a.rs` exercises, which is why its
/// `assert_row_two_untouched` could not be a key probe until now.
#[test]
fn an_autocommit_statement_does_not_discard_an_open_raii_transactions_undo() {
    let db = int_pk_table();

    let tx = db.begin_transaction().unwrap();
    tx.execute("UPDATE t SET id = 99 WHERE id = 2").unwrap();

    // An ordinary autocommit statement, on the same handle, while the RAII
    // transaction is still open.
    db.execute("INSERT INTO t (id, v) VALUES (4, 'four')").unwrap();

    tx.rollback().unwrap();

    assert_eq!(
        rows(&db, "SELECT id, v FROM t WHERE id = 2"),
        1,
        "*** INDEX LOST A LIVE KEY *** an unrelated autocommit statement discarded the open \
         transaction's ART undo entries, so its ROLLBACK restored nothing"
    );
    assert_eq!(rows(&db, "SELECT id FROM t WHERE id = 99"), 0);
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 4"),
        1,
        "the autocommit INSERT must survive the other transaction's rollback"
    );
    assert_index_agrees_with_scan(&db, "autocommit interleaved with an open RAII transaction");
}

/// BOTH PATHS. The session-transaction route (the wire's BEGIN/ROLLBACK, which
/// HAS a session id and therefore a per-session undo log replayed by
/// `finish_session_art_undo`) was already correct; this pins the two routes to
/// the same observable outcome so they cannot diverge again.
#[test]
fn session_transaction_route_agrees_with_the_raii_route() {
    // Rollback.
    let db = int_pk_table();
    let session = db.create_wire_session("writer").unwrap();
    db.execute_for_session(session, "BEGIN").unwrap();
    db.execute_for_session(session, "UPDATE t SET id = 99 WHERE id = 2")
        .unwrap();
    db.execute_for_session(session, "ROLLBACK").unwrap();

    assert_eq!(rows(&db, "SELECT id FROM t"), 2);
    assert_eq!(
        rows(&db, "SELECT id, v FROM t WHERE id = 2"),
        1,
        "session ROLLBACK must restore the original key, exactly as the RAII handle now does"
    );
    assert_eq!(rows(&db, "SELECT id FROM t WHERE id = 99"), 0);
    assert_index_agrees_with_scan(&db, "session route, after ROLLBACK");
    db.destroy_session(session).unwrap();

    // Commit.
    let db = int_pk_table();
    let session = db.create_wire_session("writer").unwrap();
    db.execute_for_session(session, "BEGIN").unwrap();
    db.execute_for_session(session, "UPDATE t SET id = 99 WHERE id = 2")
        .unwrap();
    db.execute_for_session(session, "COMMIT").unwrap();

    assert_eq!(rows(&db, "SELECT id FROM t"), 2);
    assert_eq!(
        rows(&db, "SELECT id FROM t WHERE id = 99"),
        1,
        "session COMMIT must keep the new key"
    );
    assert_eq!(rows(&db, "SELECT id FROM t WHERE id = 2"), 0);
    assert_index_agrees_with_scan(&db, "session route, after COMMIT");
    db.destroy_session(session).unwrap();
}
