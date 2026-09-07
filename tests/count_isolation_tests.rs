//! COUNT(*) isolation: no session may count another session's uncommitted rows.
//!
//! `COUNT(*)` used to be answered from `art_index_manager.pk_index_len(table)` —
//! the raw length of the primary-key ART index. That index is maintained
//! EAGERLY (an INSERT is in it at statement time so a PK conflict fails
//! immediately; a DELETE is out of it at statement time), while the row itself
//! does not reach `data:` until COMMIT. The only guard was
//! `EmbeddedDatabase::in_transaction()`, which reads the GLOBAL transaction
//! slot — and every wire connection has used a per-SESSION transaction since
//! R0.1. So the guard was blind to the transactions this database actually
//! runs, and a second connection counted rows that were never committed
//! (sprinter ca2bd77d03d8).
//!
//! The fix is a census of the staged writes that can make an index answer for
//! rows nobody else may see. It is deliberately narrow, because the index is
//! also the hot read path:
//!
//! * PER TABLE — a writer on `orders` says nothing about the index for `users`.
//!   `has_uncommitted_writes()` stays the one-atomic-load "nothing staged
//!   anywhere" fast-out; the per-table record is consulted only after it fires.
//! * PER KIND — a staged INSERT can only ADD a key, so it can never make a probe
//!   MISS; only a staged DELETE (or an UPDATE that moves an indexed value) can
//!   make a still-committed row look absent. So cardinality declines on any
//!   staged write for the table (`has_uncommitted_writes_for_table`), while
//!   missing-key lookups decline only on staged removals
//!   (`has_uncommitted_index_removals_for_table`).
//!
//! When a gate fires the answer comes from row storage instead — the committed
//! `data:` walk, or, for the transaction that staged the writes, the
//! write-set-merging scan that is the only path knowing whose transaction is
//! asking.
//!
//! These are the embedded-level tests; the wire-level ones live in
//! `src/protocol/postgres/wire_tests.rs`.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'committed')").unwrap();
    db
}

/// Two tables, so a writer on one can be shown not to affect the other.
fn setup_two_tables() -> EmbeddedDatabase {
    let db = setup();
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO u (id, v) VALUES (1, 'u-one')").unwrap();
    db.execute("INSERT INTO u (id, v) VALUES (2, 'u-two')").unwrap();
    db
}

fn as_count(value: &Value) -> i64 {
    match value {
        Value::Int8(v) => *v,
        Value::Int4(v) => i64::from(*v),
        Value::Int2(v) => i64::from(*v),
        other => panic!("COUNT(*) returned a non-integer value: {:?}", other),
    }
}

/// COUNT through the embedded (no-session) entry point — the path that owns
/// `try_fast_count_pk_query`.
fn embedded_count(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    assert_eq!(rows.len(), 1, "COUNT must return exactly one row: {}", sql);
    as_count(&rows[0].values[0])
}

/// COUNT through a wire session with no transaction of its own — the path that
/// reaches the executor's PK-cardinality fast paths.
fn session_count(db: &EmbeddedDatabase, session: heliosdb_nano::session::SessionId, sql: &str) -> i64 {
    let (rows, _) = db.query_with_columns_for_session(session, sql).unwrap();
    assert_eq!(rows.len(), 1, "COUNT must return exactly one row: {}", sql);
    as_count(&rows[0].values[0])
}

// ---------------------------------------------------------------------------
// The narrowing. The gate is per TABLE and per KIND, and both halves are now
// load-bearing: gating on "any staged write anywhere" was correct but turned a
// single open writer into a full scan for every missing-key lookup in the
// process. These assert the BEHAVIOUR of the narrowing — the answers, and the
// census predicates that decide which path may serve them — never timings.
// ---------------------------------------------------------------------------

/// A staged INSERT can only ADD an index key, so it can never make a probe
/// MISS. A missing-key lookup must stay both correct AND on its fast path while
/// another session holds a staged insert.
#[test]
fn a_staged_insert_does_not_gate_missing_key_lookups() {
    let db = setup();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (2, 'staged')")
        .unwrap();

    // The census knows something is staged for `t`…
    assert!(db.storage.has_uncommitted_writes(), "the writer must be counted");
    assert!(
        db.storage.has_uncommitted_writes_for_table("t"),
        "a staged INSERT is a staged write for the table (counts must decline)"
    );
    // …but nothing has been REMOVED from `t`'s index, so a miss is still
    // authoritative and the lookup fast paths stay available.
    assert!(
        !db.storage.has_uncommitted_index_removals_for_table("t"),
        "a staged INSERT must NOT gate missing-key lookups — it can only add keys"
    );

    // And the answers are right: the absent key is absent, the staged key is
    // invisible to the observer, and both are visible to the writer.
    assert_eq!(db.query("SELECT * FROM t WHERE id = 99", &[]).unwrap().len(), 0);
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 99")
        .unwrap();
    assert_eq!(rows.len(), 0);
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 0, "the observer must not see the staged row");
    let (rows, _) = db
        .query_with_columns_for_session(writer, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 1, "the writer must see its own staged row");

    db.execute_for_session(writer, "ROLLBACK").unwrap();
    assert!(!db.storage.has_uncommitted_writes());
    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

/// A staged DELETE is the one thing that CAN make a probe miss for a
/// still-committed row, so it must gate missing-key lookups on its table.
#[test]
fn a_staged_delete_gates_missing_key_lookups() {
    let db = setup();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'doomed')").unwrap();
    let writer = db.create_wire_session("writer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "DELETE FROM t WHERE id = 2").unwrap();

    assert!(
        db.storage.has_uncommitted_index_removals_for_table("t"),
        "a staged DELETE must gate missing-key lookups on its table"
    );
    assert!(db.storage.has_uncommitted_writes_for_table("t"));
    // The committed row is still there for everyone else (the reason for the gate).
    assert_eq!(db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap().len(), 1);

    db.execute_for_session(writer, "ROLLBACK").unwrap();
    assert!(
        !db.storage.has_uncommitted_index_removals_for_table("t"),
        "ROLLBACK must release the removal half of the census"
    );
    db.destroy_session(writer).unwrap();
}

/// An UPDATE that MOVES an indexed value breaks the index in BOTH directions at
/// once, and this covers both:
///
/// * the old key is REMOVED at statement time — which the staged write set
///   cannot reveal, since an UPDATE stages an ordinary row value. The removal
///   half of the census is fed from the ART undo funnel precisely for this case,
///   so a probe for the old key declines instead of reporting "no such row".
/// * the new key is ADDED at statement time, pointing at a row that still holds
///   the OLD value in committed storage. A probe for the new key therefore HITS
///   and materialises a committed row that does not carry the probed key. No
///   census can help there — the fix is to treat the index as a hint and verify
///   the materialised row (`fast_row_matches_probed_pk`).
#[test]
fn an_update_that_moves_an_indexed_value_gates_missing_key_lookups() {
    let db = setup();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'movable')").unwrap();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    let updated = db
        .execute_for_session(writer, "UPDATE t SET id = 99 WHERE id = 2")
        .unwrap();
    assert_eq!(updated, 1, "the UPDATE must have moved exactly one row");

    assert!(
        db.storage.has_uncommitted_index_removals_for_table("t"),
        "an UPDATE that moves an indexed value removes the old key, and must gate misses"
    );

    // The observer still matches the row under its committed key…
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 1, "an uncommitted UPDATE must not move the row for others");
    assert_eq!(db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap().len(), 1);

    // …and must NOT match it under the staged one. This is the HIT half of the
    // same defect: the index entry has already moved to 99, so a probe there
    // hits and materialises a perfectly committed row — whose `id` is still 2.
    // The content is committed; the MATCH is fabricated from uncommitted index
    // state, and answering it would leak that some open transaction has moved a
    // row onto 99. The index is a hint; the row is the truth.
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 99")
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "the observer must not see the staged new key (got a row: {:?})",
        rows.first().map(|r| r.values.clone())
    );
    assert_eq!(
        db.query("SELECT * FROM t WHERE id = 99", &[]).unwrap().len(),
        0,
        "the embedded PK fast path must not match a row that does not carry the probed key"
    );

    // The OWNER sees the move it made: new key matches, old key does not.
    let (rows, _) = db
        .query_with_columns_for_session(writer, "SELECT * FROM t WHERE id = 99")
        .unwrap();
    assert_eq!(rows.len(), 1, "the writer must see its own row under the new key");
    let (rows, _) = db
        .query_with_columns_for_session(writer, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 0, "the writer must not see its own row under the old key");

    db.execute_for_session(writer, "ROLLBACK").unwrap();
    assert!(!db.storage.has_uncommitted_index_removals_for_table("t"));
    assert_eq!(db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap().len(), 1);
    assert_eq!(db.query("SELECT * FROM t WHERE id = 99", &[]).unwrap().len(), 0);

    // Now COMMIT the same move: the truth flips for everyone.
    db.execute_for_session(writer, "BEGIN").unwrap();
    assert_eq!(
        db.execute_for_session(writer, "UPDATE t SET id = 99 WHERE id = 2")
            .unwrap(),
        1
    );
    db.execute_for_session(writer, "COMMIT").unwrap();

    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 99")
        .unwrap();
    assert_eq!(rows.len(), 1, "the committed move must be visible under the new key");
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 0, "the committed move must vacate the old key");
    assert_eq!(db.query("SELECT * FROM t WHERE id = 99", &[]).unwrap().len(), 1);
    assert_eq!(db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap().len(), 0);

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

/// A writer on one table must not gate ANY fast path on another table — not the
/// counts, not the missing-key lookups. This is the whole point of the per-table
/// census: a writer on `orders` must not slow down reads on `users`.
#[test]
fn a_writer_on_one_table_does_not_gate_another_table() {
    let db = setup_two_tables();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (2, 'staged')")
        .unwrap();
    db.execute_for_session(writer, "DELETE FROM t WHERE id = 1").unwrap();

    // `t` is gated in both halves…
    assert!(db.storage.has_uncommitted_writes_for_table("t"));
    assert!(db.storage.has_uncommitted_index_removals_for_table("t"));
    // …and `u` in neither.
    assert!(
        !db.storage.has_uncommitted_writes_for_table("u"),
        "a writer on `t` must not gate counts on `u`"
    );
    assert!(
        !db.storage.has_uncommitted_index_removals_for_table("u"),
        "a writer on `t` must not gate missing-key lookups on `u`"
    );

    // …and every answer about `u` is right, from both connections.
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM u"), 2);
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM u"), 2);
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM u WHERE id = 1"), 1);
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM u WHERE id >= 1"), 2);
    assert_eq!(db.query("SELECT * FROM u WHERE id = 99", &[]).unwrap().len(), 0);
    assert_eq!(db.query("SELECT * FROM u WHERE id = 2", &[]).unwrap().len(), 1);

    // The gated table still answers correctly, just not from the index.
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 1);
    assert_eq!(db.query("SELECT * FROM t WHERE id = 1", &[]).unwrap().len(), 1);

    db.execute_for_session(writer, "COMMIT").unwrap();
    assert!(!db.storage.has_uncommitted_writes());
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM u"), 2);
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 1);

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

/// The per-table and per-kind slots must be as exact as the global one: every
/// way a transaction can end has to release both halves, for every table it
/// touched.
#[test]
fn per_table_census_slots_are_released_on_every_transaction_ending() {
    let db = setup_two_tables();

    for (i, ending) in ["COMMIT", "ROLLBACK", "destroy"].iter().enumerate() {
        let s = db.create_wire_session("looper").unwrap();
        db.execute_for_session(s, "BEGIN").unwrap();
        db.execute_for_session(s, &format!("INSERT INTO t (id, v) VALUES ({}, 'x')", 100 + i))
            .unwrap();
        db.execute_for_session(s, &format!("INSERT INTO u (id, v) VALUES ({}, 'x')", 100 + i))
            .unwrap();
        db.execute_for_session(s, "DELETE FROM u WHERE id = 1").unwrap();

        assert!(db.storage.has_uncommitted_writes_for_table("t"), "{}", ending);
        assert!(db.storage.has_uncommitted_writes_for_table("u"), "{}", ending);
        assert!(
            !db.storage.has_uncommitted_index_removals_for_table("t"),
            "insert-only table must not be marked as having removals ({})",
            ending
        );
        assert!(db.storage.has_uncommitted_index_removals_for_table("u"), "{}", ending);

        if *ending == "COMMIT" {
            db.execute_for_session(s, "COMMIT").unwrap();
        } else if *ending == "ROLLBACK" {
            db.execute_for_session(s, "ROLLBACK").unwrap();
        } // "destroy": left open, released by destroy_session below.
        db.destroy_session(s).unwrap();

        assert!(
            !db.storage.has_uncommitted_writes(),
            "global census leaked after {}",
            ending
        );
        for table in ["t", "u"] {
            assert!(
                !db.storage.has_uncommitted_writes_for_table(table),
                "`{}` write slot leaked after {}",
                table,
                ending
            );
            assert!(
                !db.storage.has_uncommitted_index_removals_for_table(table),
                "`{}` removal slot leaked after {}",
                table,
                ending
            );
        }
        // Restore `u`'s deleted row for the next iteration when the DELETE stuck.
        if *ending == "COMMIT" {
            db.execute("INSERT INTO u (id, v) VALUES (1, 'u-one')").unwrap();
        }
    }
}

/// NOT a count — kept here because it is the same defect, one layer down, and
/// is closed by the same guard (`fast_lookup_miss_is_authoritative` in `lib.rs`
/// / `index_miss_is_authoritative` in the executor's scan paths).
///
/// An uncommitted DELETE strips the row's PK ART entry at STATEMENT time, so
/// every index-driven lookup reported a still-committed row as absent — to
/// sessions that had nothing to do with the deleting transaction, and even if
/// that transaction later rolled back. This is the reason the COUNT fix had to
/// reach the index probes rather than stopping at the count fast paths: the
/// predicate form of `count(*)` is *implemented by* those probes.
#[test]
fn value_lookup_sees_a_row_another_session_has_deleted_but_not_committed() {
    let db = setup();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'doomed')").unwrap();
    let deleter = db.create_wire_session("deleter").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(deleter, "BEGIN").unwrap();
    db.execute_for_session(deleter, "DELETE FROM t WHERE id = 2").unwrap();

    // The observer, through the wire session path…
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "an observer lost a COMMITTED row to another session's uncommitted DELETE"
    );
    assert!(
        matches!(&rows[0].values[1], Value::String(v) if v == "doomed"),
        "the committed row must come back intact, got {:?}",
        rows[0].values
    );

    // …the embedded literal fast path (`try_fast_select` / `fast_select_rows`)…
    let rows = db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the embedded PK fast path lost a COMMITTED row to an uncommitted DELETE"
    );

    // …and the projected form, which routes through the executor's index probe.
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT v FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the executor index probe lost a COMMITTED row to an uncommitted DELETE"
    );

    // The deleting session itself must NOT see it — its own DELETE is not a
    // dirty read.
    let (rows, _) = db
        .query_with_columns_for_session(deleter, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 0, "the deleting session must not see the row it deleted");

    // ROLLBACK: the row was never gone, for anyone.
    db.execute_for_session(deleter, "ROLLBACK").unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(deleter, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 1, "ROLLBACK restores the row for the deleter too");
    assert_eq!(db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap().len(), 1);

    // COMMIT: now it is gone for everyone.
    db.execute_for_session(deleter, "BEGIN").unwrap();
    db.execute_for_session(deleter, "DELETE FROM t WHERE id = 2").unwrap();
    db.execute_for_session(deleter, "COMMIT").unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(observer, "SELECT * FROM t WHERE id = 2")
        .unwrap();
    assert_eq!(rows.len(), 0, "the committed DELETE must be visible to everyone");
    assert_eq!(db.query("SELECT * FROM t WHERE id = 2", &[]).unwrap().len(), 0);

    db.destroy_session(deleter).unwrap();
    db.destroy_session(observer).unwrap();
}

#[test]
fn count_star_does_not_see_another_sessions_uncommitted_insert() {
    let db = setup();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (2, 'uncommitted')")
        .unwrap();

    assert_eq!(
        session_count(&db, observer, "SELECT count(*) FROM t"),
        1,
        "a second session COUNTED an UNCOMMITTED row"
    );
    assert_eq!(
        embedded_count(&db, "SELECT count(*) FROM t"),
        1,
        "the embedded COUNT fast path COUNTED an UNCOMMITTED row"
    );

    db.execute_for_session(writer, "COMMIT").unwrap();

    assert_eq!(
        session_count(&db, observer, "SELECT count(*) FROM t"),
        2,
        "the committed row must be counted"
    );
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 2);

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

#[test]
fn count_pk_predicate_does_not_see_another_sessions_uncommitted_insert() {
    let db = setup();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (2, 'uncommitted')")
        .unwrap();

    // The `WHERE pk = …` form is answered by a direct ART probe
    // (`pk_index_contains` / `pk_index_count_int_range`), not by `pk_index_len`.
    for sql in [
        "SELECT count(*) FROM t WHERE id = 2",
        "SELECT count(*) FROM t WHERE id IN (2)",
        "SELECT count(*) FROM t WHERE id >= 2",
    ] {
        assert_eq!(
            session_count(&db, observer, sql),
            0,
            "a second session counted an UNCOMMITTED row via `{}`",
            sql
        );
        assert_eq!(
            embedded_count(&db, sql),
            0,
            "the embedded COUNT fast path counted an UNCOMMITTED row via `{}`",
            sql
        );
    }

    db.execute_for_session(writer, "ROLLBACK").unwrap();

    for sql in [
        "SELECT count(*) FROM t WHERE id = 2",
        "SELECT count(*) FROM t WHERE id IN (2)",
        "SELECT count(*) FROM t WHERE id >= 2",
    ] {
        assert_eq!(session_count(&db, observer, sql), 0, "`{}` after ROLLBACK", sql);
    }

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

#[test]
fn count_star_does_not_see_rows_a_rollback_removes() {
    let db = setup();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (2, 'never-committed')")
        .unwrap();
    let during = session_count(&db, observer, "SELECT count(*) FROM t");
    db.execute_for_session(writer, "ROLLBACK").unwrap();
    let after = session_count(&db, observer, "SELECT count(*) FROM t");

    assert_eq!(
        during, 1,
        "the observer counted a row that ROLLBACK then removed — it never existed"
    );
    assert_eq!(after, 1, "the rolled-back row must not be counted afterwards");

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

/// The OTHER half of the contract, and the one that makes this hard: the
/// session that OWNS the transaction must still see its own staged rows. Its
/// own writes are not a dirty read, and the fix must not buy the observer's
/// correctness with the owner's read-your-own-writes.
///
/// The index used to give the owner the right answer by accident — it is the
/// asking transaction's dirty view (its inserts added, its deletes stripped)
/// and simultaneously everyone else's. The owner's answer now comes from the
/// write-set-merging scan instead, which is the only path that knows *whose*
/// transaction is asking.
#[test]
fn owner_sees_its_own_uncommitted_insert_in_count() {
    let db = setup();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (2, 'mine')")
        .unwrap();

    assert_eq!(
        session_count(&db, writer, "SELECT count(*) FROM t"),
        2,
        "the writing session must see its own uncommitted row"
    );
    for sql in [
        "SELECT count(*) FROM t WHERE id = 2",
        "SELECT count(*) FROM t WHERE id IN (2)",
        "SELECT count(*) FROM t WHERE id >= 2",
    ] {
        assert_eq!(
            session_count(&db, writer, sql),
            1,
            "the writing session must see its own uncommitted row via `{}`",
            sql
        );
    }

    // …while the observer, at the very same moment, must not.
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 1);
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t WHERE id = 2"), 0);

    db.execute_for_session(writer, "COMMIT").unwrap();
    assert_eq!(session_count(&db, writer, "SELECT count(*) FROM t"), 2);
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 2);

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

/// Owner visibility in the other direction: a row this transaction has DELETEd
/// is gone for it, and still there for everyone else, until COMMIT.
#[test]
fn owner_sees_its_own_uncommitted_delete_in_count() {
    let db = setup();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'doomed')").unwrap();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "DELETE FROM t WHERE id = 2").unwrap();

    assert_eq!(
        session_count(&db, writer, "SELECT count(*) FROM t"),
        1,
        "the deleting session must not count the row it just deleted"
    );
    for sql in [
        "SELECT count(*) FROM t WHERE id = 2",
        "SELECT count(*) FROM t WHERE id IN (2)",
        "SELECT count(*) FROM t WHERE id >= 2",
    ] {
        assert_eq!(
            session_count(&db, writer, sql),
            0,
            "the deleting session must not count its deleted row via `{}`",
            sql
        );
    }

    // …while the observer still sees the committed row.
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 2);
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t WHERE id = 2"), 1);

    db.execute_for_session(writer, "ROLLBACK").unwrap();
    assert_eq!(
        session_count(&db, writer, "SELECT count(*) FROM t"),
        2,
        "ROLLBACK restores the row for the deleter too"
    );
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 2);

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

/// The embedded global-slot transaction is the same owner contract on a
/// different writer shape: `db.execute("BEGIN")` then `db.query(...)`.
#[test]
fn owner_sees_its_own_uncommitted_rows_through_the_global_transaction() {
    let db = setup();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'mine')").unwrap();
    assert_eq!(
        embedded_count(&db, "SELECT count(*) FROM t"),
        2,
        "the embedded transaction must see its own uncommitted row"
    );
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 1);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 1);

    db.destroy_session(observer).unwrap();
}

/// A count can be wrong in BOTH directions: the ART entry of a DELETEd row is
/// stripped at statement time, so the index under-reports before COMMIT.
#[test]
fn count_star_does_not_see_another_sessions_uncommitted_delete() {
    let db = setup();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'doomed')").unwrap();
    let writer = db.create_wire_session("writer").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "DELETE FROM t WHERE id = 2").unwrap();

    assert_eq!(
        session_count(&db, observer, "SELECT count(*) FROM t"),
        2,
        "a second session lost a row to an UNCOMMITTED DELETE"
    );
    assert_eq!(
        embedded_count(&db, "SELECT count(*) FROM t"),
        2,
        "the embedded COUNT fast path lost a row to an UNCOMMITTED DELETE"
    );
    assert_eq!(
        session_count(&db, observer, "SELECT count(*) FROM t WHERE id = 2"),
        1,
        "the PK predicate form lost a row to an UNCOMMITTED DELETE"
    );

    db.execute_for_session(writer, "ROLLBACK").unwrap();
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 2);

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "DELETE FROM t WHERE id = 2").unwrap();
    db.execute_for_session(writer, "COMMIT").unwrap();
    assert_eq!(
        session_count(&db, observer, "SELECT count(*) FROM t"),
        1,
        "the committed DELETE must be reflected"
    );

    db.destroy_session(writer).unwrap();
    db.destroy_session(observer).unwrap();
}

/// Positive control: with no transaction open anywhere, the gate is open and
/// the count is right. The only signal this build exposes for "the index fast
/// path is available" is the census itself, so that is what is asserted —
/// nothing here invents a new one.
#[test]
fn count_star_is_correct_and_ungated_with_no_transaction_open() {
    let db = setup();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'two')").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (3, 'three')").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    assert!(
        !db.storage.has_uncommitted_writes(),
        "autocommit writes must not leave the census armed"
    );
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 3);
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 3);
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t WHERE id = 2"), 1);
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t WHERE id > 1"), 2);
    assert!(!db.storage.has_uncommitted_writes(), "reads must not arm the census");

    db.destroy_session(observer).unwrap();
}

/// A session sitting in `BEGIN` that has only READ cannot change any count, so
/// it must not disable the fast path for anyone.
#[test]
fn read_only_session_transaction_does_not_arm_the_census() {
    let db = setup();
    let reader = db.create_wire_session("reader").unwrap();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute_for_session(reader, "BEGIN").unwrap();
    let (rows, _) = db.query_with_columns_for_session(reader, "SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(session_count(&db, reader, "SELECT count(*) FROM t"), 1);

    assert!(
        !db.storage.has_uncommitted_writes(),
        "a read-only session transaction must not arm the write census"
    );
    assert_eq!(session_count(&db, observer, "SELECT count(*) FROM t"), 1);
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 1);

    db.execute_for_session(reader, "COMMIT").unwrap();
    assert!(!db.storage.has_uncommitted_writes());

    db.destroy_session(reader).unwrap();
    db.destroy_session(observer).unwrap();
}

/// The census must be EXACT: every way a session transaction can end has to
/// return it to zero. A leak upward disables the fast path forever; a leak
/// downward re-opens the dirty read.
#[test]
fn write_census_returns_to_zero_on_every_transaction_ending() {
    let db = setup();
    let census = db.storage.write_census();
    assert_eq!(census.uncommitted_write_transactions(), 0);

    // COMMIT
    let s = db.create_wire_session("commit").unwrap();
    db.execute_for_session(s, "BEGIN").unwrap();
    db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (10, 'a')")
        .unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 1, "staged write must count");
    db.execute_for_session(s, "COMMIT").unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 0, "COMMIT must release");
    db.destroy_session(s).unwrap();

    // ROLLBACK
    let s = db.create_wire_session("rollback").unwrap();
    db.execute_for_session(s, "BEGIN").unwrap();
    db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (11, 'b')")
        .unwrap();
    db.execute_for_session(s, "ROLLBACK").unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 0, "ROLLBACK must release");
    db.destroy_session(s).unwrap();

    // Dropped connection with a transaction still open (the wire handler's
    // `Drop` calls `destroy_session`).
    let s = db.create_wire_session("dropped").unwrap();
    db.execute_for_session(s, "BEGIN").unwrap();
    db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (12, 'c')")
        .unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 1);
    db.destroy_session(s).unwrap();
    assert_eq!(
        census.uncommitted_write_transactions(),
        0,
        "destroy_session must release the census of the transaction it rolls back"
    );

    // A failed statement inside a transaction, then ROLLBACK.
    let s = db.create_wire_session("failed").unwrap();
    db.execute_for_session(s, "BEGIN").unwrap();
    db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (13, 'd')")
        .unwrap();
    let _ = db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (1, 'dup-pk')");
    db.execute_for_session(s, "ROLLBACK").unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 0);
    db.destroy_session(s).unwrap();

    // Many transactions in a row, ended every which way.
    for i in 0..25i64 {
        let s = db.create_wire_session("loop").unwrap();
        db.execute_for_session(s, "BEGIN").unwrap();
        db.execute_for_session(s, &format!("INSERT INTO t (id, v) VALUES ({}, 'x')", 100 + i))
            .unwrap();
        if i % 3 == 0 {
            db.execute_for_session(s, "COMMIT").unwrap();
        } else if i % 3 == 1 {
            db.execute_for_session(s, "ROLLBACK").unwrap();
        } // else: left open, and released by `destroy_session` below.
        db.destroy_session(s).unwrap();
        assert_eq!(
            census.uncommitted_write_transactions(),
            0,
            "census leaked after iteration {}",
            i
        );
    }

    // …and the fast path is available again, with the right answer.
    assert!(!db.storage.has_uncommitted_writes());
    let expected = embedded_count(&db, "SELECT count(*) FROM t");
    let rows = db.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(expected, rows.len() as i64, "COUNT(*) must match the rows on hand");
}

/// Concurrent open transactions each hold one census slot, and only the last
/// one to end clears it.
#[test]
fn write_census_counts_each_writing_transaction_once() {
    let db = setup();
    let census = db.storage.write_census();
    let a = db.create_wire_session("a").unwrap();
    let b = db.create_wire_session("b").unwrap();

    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(b, "BEGIN").unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 0, "BEGIN alone is not a write");

    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (2, 'a')")
        .unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (3, 'a')")
        .unwrap();
    assert_eq!(
        census.uncommitted_write_transactions(),
        1,
        "a transaction is counted ONCE however many rows it stages"
    );

    db.execute_for_session(b, "INSERT INTO t (id, v) VALUES (4, 'b')")
        .unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 2);

    db.execute_for_session(a, "COMMIT").unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 1);
    // Still gated: B is open, so nobody may read the index for a count.
    assert!(db.storage.has_uncommitted_writes());
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 3);

    db.execute_for_session(b, "ROLLBACK").unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 0);
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 3);

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

/// The result cache must not preserve a count across the event that corrected
/// it. A count computed while a writer was mid-transaction is published only if
/// nothing invalidated the cache in the meantime; a COMMIT/ROLLBACK invalidates.
#[test]
fn cached_count_does_not_survive_the_commit_that_corrects_it() {
    let db = setup();
    let writer = db.create_wire_session("writer").unwrap();

    // Warm the cache on the committed state.
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 1);
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 1);

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (2, 'pending')")
        .unwrap();

    // Counted while the writer holds staged writes: must be the committed count…
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 1);

    db.execute_for_session(writer, "COMMIT").unwrap();

    // …and must NOT be served from the cache after the commit.
    assert_eq!(
        embedded_count(&db, "SELECT count(*) FROM t"),
        2,
        "the result cache served a count from before the COMMIT that corrected it"
    );

    db.execute_for_session(writer, "BEGIN").unwrap();
    db.execute_for_session(writer, "INSERT INTO t (id, v) VALUES (3, 'pending')")
        .unwrap();
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 2);
    db.execute_for_session(writer, "ROLLBACK").unwrap();
    assert_eq!(
        embedded_count(&db, "SELECT count(*) FROM t"),
        2,
        "a rolled-back row must never appear, cached or not"
    );

    db.destroy_session(writer).unwrap();
}

/// The embedded global-slot transaction (`db.execute("BEGIN")`) is the other
/// writer shape: its rows must not be counted by a concurrent session either,
/// and its ROLLBACK must not leave the index over-reporting.
#[test]
fn global_transaction_uncommitted_rows_are_not_counted_by_a_session() {
    let db = setup();
    let observer = db.create_wire_session("observer").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (2, 'global-uncommitted')")
        .unwrap();
    assert_eq!(
        session_count(&db, observer, "SELECT count(*) FROM t"),
        1,
        "a session counted the embedded global transaction's uncommitted row"
    );
    db.execute("ROLLBACK").unwrap();

    assert!(!db.storage.has_uncommitted_writes(), "ROLLBACK must release the census");
    assert_eq!(
        session_count(&db, observer, "SELECT count(*) FROM t"),
        1,
        "the rolled-back row must not be counted"
    );
    assert_eq!(embedded_count(&db, "SELECT count(*) FROM t"), 1);

    db.destroy_session(observer).unwrap();
}
