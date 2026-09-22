//! GH#41 / sprinter 27bf8d819c52 — READ-YOUR-OWN-WRITES ON THE **WRITE** PATH.
//!
//! Inside an explicit transaction, `UPDATE` and `DELETE` could not see rows the
//! SAME transaction had just `INSERT`ed. They matched 0 rows, reported rowcount
//! 0, raised no error, and the intended change was silently discarded — while a
//! `SELECT` in the same transaction saw those rows perfectly well. The read path
//! and the DML write path disagreed about the transaction's own write set.
//!
//! ## Why it happened (pinned here so the tests read as a contract)
//!
//! An `INSERT` inside an explicit transaction stages its ART index key eagerly
//! but stages the ROW in the transaction's `write_set` / `insert_log`; the row
//! does not reach `data:` until COMMIT (`storage/transaction.rs`, the
//! `UncommittedWriteCensus` doc block). The **params / extended-protocol**
//! `Update`/`Delete` arms therefore scan and then call
//! `Transaction::merge_with_write_set`. The **text / simple-query** arms did
//! not: they read `data:` directly, via a PK point lookup when the predicate
//! was `pk = literal` and a plain `scan_table_branch_aware` otherwise. Both
//! sources are blind to a staged row, so:
//!
//!   * PK predicate      → point lookup misses `data:` → `None => vec![]` → 0
//!   * non-key predicate → scan of `data:` lacks the row → 0 matches
//!   * pre-existing row  → present in `data:` → 1 (worked, which is why this
//!                         survived: every "does UPDATE work in a txn?" test
//!                         used a committed row)
//!   * autocommit        → the INSERT committed before the UPDATE ran (worked)
//!
//! ## What this file pins
//!
//! The full corpus the remediation asked for, driven through BOTH families and
//! BOTH connection shapes (embedded global `BEGIN` and a wire session
//! transaction), because the two families dispatch through different code:
//!
//!   {UPDATE, DELETE} × {key predicate, non-key predicate}
//!                    × {table WITH a PK, table WITHOUT one}
//!                    × {text, bound params} × {embedded, session}
//!
//! plus a pre-existing-row control, an autocommit control, and — the direction
//! that must NOT be traded away for the fix — a second session that still sees
//! nothing of the open transaction's staged rows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::session::SessionId;
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn int_of(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected an integer column value, got {other:?}"),
    }
}

/// `(id, n)` pairs, sorted, so a result is comparable regardless of scan order.
fn pairs(rows: &[Tuple]) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = rows
        .iter()
        .map(|t| {
            assert!(
                t.values.len() >= 2,
                "expected two projected columns, got {:?}",
                t.values
            );
            (int_of(&t.values[0]), int_of(&t.values[1]))
        })
        .collect();
    out.sort_unstable();
    out
}

/// Which DML dispatch family the statement goes through. These are genuinely
/// separate implementations in `src/lib.rs` — the whole point of the bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `execute()` / `execute_for_session()` — simple query, no bound params.
    Text,
    /// `execute_params()` / `execute_params_for_session()` — extended protocol.
    Params,
}

/// Which transaction slot holds the open transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Conn {
    /// The embedded handle's global `current_transaction` slot.
    Embedded,
    /// A wire session's per-session transaction slot.
    Session,
}

const DRIVERS: [(Conn, Family); 4] = [
    (Conn::Embedded, Family::Text),
    (Conn::Embedded, Family::Params),
    (Conn::Session, Family::Text),
    (Conn::Session, Family::Params),
];

struct Driver {
    db: EmbeddedDatabase,
    session: Option<SessionId>,
    family: Family,
    label: String,
}

impl Driver {
    fn open(conn: Conn, family: Family) -> Self {
        let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
        let session = match conn {
            Conn::Embedded => None,
            Conn::Session => Some(db.create_wire_session("gh41").expect("wire session")),
        };
        Self {
            db,
            session,
            family,
            label: format!("{conn:?}/{family:?}"),
        }
    }

    /// Autocommit setup (DDL and any pre-existing rows), always through the
    /// embedded handle so it is committed before the transaction under test.
    fn setup(&self, sql: &str) {
        self.db
            .execute(sql)
            .unwrap_or_else(|e| panic!("{}: setup `{sql}`: {e}", self.label));
    }

    /// Transaction control always travels as simple text — that is how every
    /// real driver sends `BEGIN`/`COMMIT`, including the extended-protocol ones.
    fn txn_control(&self, sql: &str) {
        match self.session {
            Some(sid) => self.db.execute_for_session(sid, sql),
            None => self.db.execute(sql),
        }
        .unwrap_or_else(|e| panic!("{}: `{sql}`: {e}", self.label));
    }

    fn begin(&self) {
        self.txn_control("BEGIN");
    }

    fn commit(&self) {
        self.txn_control("COMMIT");
    }

    fn rollback(&self) {
        self.txn_control("ROLLBACK");
    }

    /// One DML statement through the family under test. `text_sql` carries
    /// literals; `param_sql` carries `$n` placeholders bound from `params`.
    fn dml(&self, text_sql: &str, param_sql: &str, params: &[Value]) -> u64 {
        let result = match (self.session, self.family) {
            (None, Family::Text) => self.db.execute(text_sql),
            (None, Family::Params) => self.db.execute_params(param_sql, params),
            (Some(sid), Family::Text) => self.db.execute_for_session(sid, text_sql),
            (Some(sid), Family::Params) => self.db.execute_params_for_session(sid, param_sql, params),
        };
        result.unwrap_or_else(|e| panic!("{}: `{text_sql}`: {e}", self.label))
    }

    fn insert(&self, table: &str, id: i32, n: i32) {
        let count = self.dml(
            &format!("INSERT INTO {table} (id, n) VALUES ({id}, {n})"),
            &format!("INSERT INTO {table} (id, n) VALUES ($1, $2)"),
            &[Value::Int4(id), Value::Int4(n)],
        );
        assert_eq!(count, 1, "{}: INSERT INTO {table} must report one row", self.label);
    }

    /// Read through the SAME connection, so an open transaction is attached and
    /// the read sees its own writes.
    fn read(&self, sql: &str) -> Vec<(i64, i64)> {
        let rows = match self.session {
            Some(sid) => {
                self.db
                    .query_with_columns_for_session(sid, sql)
                    .unwrap_or_else(|e| panic!("{}: `{sql}`: {e}", self.label))
                    .0
            }
            None => self
                .db
                .query(sql, &[])
                .unwrap_or_else(|e| panic!("{}: `{sql}`: {e}", self.label)),
        };
        pairs(&rows)
    }
}

const PK_DDL: &str = "CREATE TABLE t (id INT4 PRIMARY KEY, n INT4)";
/// Deliberately no PRIMARY KEY: proves the defect was never index-specific.
/// `try_extract_pk_value` declines here, so these cells only ever exercised the
/// `scan_table_branch_aware` row source.
const NOPK_DDL: &str = "CREATE TABLE t (id INT4, n INT4)";
const READ_ALL: &str = "SELECT id, n FROM t";

/// Stage two rows in the open transaction and return the driver.
fn staged(ddl: &str, conn: Conn, family: Family) -> Driver {
    let d = Driver::open(conn, family);
    d.setup(ddl);
    d.begin();
    d.insert("t", 1, 1);
    d.insert("t", 2, 2);
    // The read path already saw these. The write path is what this file is about.
    assert_eq!(
        d.read(READ_ALL),
        vec![(1, 1), (2, 2)],
        "{}: the in-transaction SELECT must see the transaction's own inserts \
         (if THIS fails the read overlay regressed, not the write path)",
        d.label
    );
    d
}

// ---------------------------------------------------------------------------
// UPDATE — table WITH a primary key
// ---------------------------------------------------------------------------

#[test]
fn update_by_pk_sees_own_insert() {
    for (conn, family) in DRIVERS {
        let d = staged(PK_DDL, conn, family);
        let updated = d.dml(
            "UPDATE t SET n = 99 WHERE id = 1",
            "UPDATE t SET n = 99 WHERE id = $1",
            &[Value::Int4(1)],
        );
        assert_eq!(
            updated, 1,
            "{}: UPDATE by PK must match the row this transaction inserted",
            d.label
        );
        assert_eq!(d.read(READ_ALL), vec![(1, 99), (2, 2)], "{}: in-transaction", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 99), (2, 2)], "{}: post-COMMIT", d.label);
    }
}

#[test]
fn update_by_non_key_predicate_sees_own_insert() {
    for (conn, family) in DRIVERS {
        let d = staged(PK_DDL, conn, family);
        let updated = d.dml(
            "UPDATE t SET n = 99 WHERE n = 1",
            "UPDATE t SET n = 99 WHERE n = $1",
            &[Value::Int4(1)],
        );
        assert_eq!(
            updated, 1,
            "{}: UPDATE by a non-key predicate must match the row this transaction inserted",
            d.label
        );
        assert_eq!(d.read(READ_ALL), vec![(1, 99), (2, 2)], "{}: in-transaction", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 99), (2, 2)], "{}: post-COMMIT", d.label);
    }
}

// ---------------------------------------------------------------------------
// DELETE — table WITH a primary key
// ---------------------------------------------------------------------------

#[test]
fn delete_by_pk_sees_own_insert() {
    for (conn, family) in DRIVERS {
        let d = staged(PK_DDL, conn, family);
        let deleted = d.dml(
            "DELETE FROM t WHERE id = 2",
            "DELETE FROM t WHERE id = $1",
            &[Value::Int4(2)],
        );
        assert_eq!(
            deleted, 1,
            "{}: DELETE by PK must match the row this transaction inserted",
            d.label
        );
        assert_eq!(d.read(READ_ALL), vec![(1, 1)], "{}: in-transaction", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 1)], "{}: post-COMMIT", d.label);
    }
}

#[test]
fn delete_by_non_key_predicate_sees_own_insert() {
    for (conn, family) in DRIVERS {
        let d = staged(PK_DDL, conn, family);
        let deleted = d.dml(
            "DELETE FROM t WHERE n = 2",
            "DELETE FROM t WHERE n = $1",
            &[Value::Int4(2)],
        );
        assert_eq!(
            deleted, 1,
            "{}: DELETE by a non-key predicate must match the row this transaction inserted",
            d.label
        );
        assert_eq!(d.read(READ_ALL), vec![(1, 1)], "{}: in-transaction", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 1)], "{}: post-COMMIT", d.label);
    }
}

// ---------------------------------------------------------------------------
// The same four cells on a table with NO primary key (PROBE 3 in the report)
// ---------------------------------------------------------------------------

#[test]
fn update_without_pk_sees_own_insert() {
    for (conn, family) in DRIVERS {
        // `WHERE id = 1` — key-SHAPED, but `id` is not a key here.
        let d = staged(NOPK_DDL, conn, family);
        let updated = d.dml(
            "UPDATE t SET n = 99 WHERE id = 1",
            "UPDATE t SET n = 99 WHERE id = $1",
            &[Value::Int4(1)],
        );
        assert_eq!(updated, 1, "{}: no-PK table, key-shaped predicate", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 99), (2, 2)], "{}: post-COMMIT", d.label);

        // `WHERE n = …` — plain non-key predicate.
        let d = staged(NOPK_DDL, conn, family);
        let updated = d.dml(
            "UPDATE t SET n = 99 WHERE n = 2",
            "UPDATE t SET n = 99 WHERE n = $1",
            &[Value::Int4(2)],
        );
        assert_eq!(updated, 1, "{}: no-PK table, non-key predicate", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 1), (2, 99)], "{}: post-COMMIT", d.label);
    }
}

#[test]
fn delete_without_pk_sees_own_insert() {
    for (conn, family) in DRIVERS {
        let d = staged(NOPK_DDL, conn, family);
        let deleted = d.dml(
            "DELETE FROM t WHERE id = 2",
            "DELETE FROM t WHERE id = $1",
            &[Value::Int4(2)],
        );
        assert_eq!(deleted, 1, "{}: no-PK table, key-shaped predicate", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 1)], "{}: post-COMMIT", d.label);

        let d = staged(NOPK_DDL, conn, family);
        let deleted = d.dml(
            "DELETE FROM t WHERE n = 1",
            "DELETE FROM t WHERE n = $1",
            &[Value::Int4(1)],
        );
        assert_eq!(deleted, 1, "{}: no-PK table, non-key predicate", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(2, 2)], "{}: post-COMMIT", d.label);
    }
}

// ---------------------------------------------------------------------------
// The create-then-patch shape ORMs emit, end to end
// ---------------------------------------------------------------------------

#[test]
fn insert_then_update_then_delete_in_one_transaction() {
    for (conn, family) in DRIVERS {
        let d = staged(PK_DDL, conn, family);
        d.insert("t", 3, 3);
        assert_eq!(
            d.dml(
                "UPDATE t SET n = 30 WHERE id = 3",
                "UPDATE t SET n = 30 WHERE id = $1",
                &[Value::Int4(3)]
            ),
            1,
            "{}: UPDATE of an own insert",
            d.label
        );
        assert_eq!(
            d.dml(
                "DELETE FROM t WHERE id = 1",
                "DELETE FROM t WHERE id = $1",
                &[Value::Int4(1)]
            ),
            1,
            "{}: DELETE of an own insert, after an UPDATE staged more writes",
            d.label
        );
        assert_eq!(d.read(READ_ALL), vec![(2, 2), (3, 30)], "{}: in-transaction", d.label);
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(2, 2), (3, 30)], "{}: post-COMMIT", d.label);
    }
}

// ---------------------------------------------------------------------------
// CONTROLS — these passed BEFORE the fix and must keep passing
// ---------------------------------------------------------------------------

/// PROBE 2 from the report: a row committed before `BEGIN` was always
/// updatable inside the transaction. Keeping a pre-existing row and an own
/// insert in the SAME transaction is what proves the repair did not swap one
/// row source for another — both must be visible to the write path at once.
#[test]
fn pre_existing_rows_still_update_and_delete_inside_a_transaction() {
    for (conn, family) in DRIVERS {
        let d = Driver::open(conn, family);
        d.setup(PK_DDL);
        d.setup("INSERT INTO t (id, n) VALUES (1, 1)");
        d.setup("INSERT INTO t (id, n) VALUES (2, 2)");

        d.begin();
        assert_eq!(
            d.dml(
                "UPDATE t SET n = 50 WHERE id = 1",
                "UPDATE t SET n = 50 WHERE id = $1",
                &[Value::Int4(1)]
            ),
            1,
            "{}: pre-existing row, PK predicate",
            d.label
        );
        // Now stage an insert too, and update THAT in the same transaction —
        // the mixed case that a single-source implementation cannot satisfy.
        d.insert("t", 3, 3);
        assert_eq!(
            d.dml(
                "UPDATE t SET n = 60 WHERE id = 3",
                "UPDATE t SET n = 60 WHERE id = $1",
                &[Value::Int4(3)]
            ),
            1,
            "{}: own insert, PK predicate, in a transaction that also touched a committed row",
            d.label
        );
        assert_eq!(
            d.dml(
                "DELETE FROM t WHERE id = 2",
                "DELETE FROM t WHERE id = $1",
                &[Value::Int4(2)]
            ),
            1,
            "{}: pre-existing row, DELETE by PK",
            d.label
        );
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 50), (3, 60)], "{}: post-COMMIT", d.label);
    }
}

/// PROBE 4 from the report: autocommit was never broken, and must not become
/// slower-but-different. The fix must not change a single autocommit answer.
#[test]
fn autocommit_insert_then_update_control() {
    for (conn, family) in DRIVERS {
        let d = Driver::open(conn, family);
        d.setup(PK_DDL);
        d.insert("t", 1, 1);
        assert_eq!(
            d.dml(
                "UPDATE t SET n = 99 WHERE id = 1",
                "UPDATE t SET n = 99 WHERE id = $1",
                &[Value::Int4(1)]
            ),
            1,
            "{}: autocommit UPDATE by PK",
            d.label
        );
        assert_eq!(d.read(READ_ALL), vec![(1, 99)], "{}: autocommit UPDATE", d.label);
        assert_eq!(
            d.dml(
                "DELETE FROM t WHERE id = 1",
                "DELETE FROM t WHERE id = $1",
                &[Value::Int4(1)]
            ),
            1,
            "{}: autocommit DELETE by PK",
            d.label
        );
        assert_eq!(
            d.read(READ_ALL),
            Vec::<(i64, i64)>::new(),
            "{}: autocommit DELETE",
            d.label
        );
    }
}

/// A predicate that genuinely matches nothing must still report 0 — the fix
/// must not turn "no such row" into a match by widening the row source.
#[test]
fn a_predicate_matching_nothing_still_reports_zero() {
    for (conn, family) in DRIVERS {
        let d = staged(PK_DDL, conn, family);
        assert_eq!(
            d.dml(
                "UPDATE t SET n = 99 WHERE id = 404",
                "UPDATE t SET n = 99 WHERE id = $1",
                &[Value::Int4(404)]
            ),
            0,
            "{}: UPDATE of an absent PK",
            d.label
        );
        assert_eq!(
            d.dml(
                "DELETE FROM t WHERE n = 404",
                "DELETE FROM t WHERE n = $1",
                &[Value::Int4(404)]
            ),
            0,
            "{}: DELETE on an unmatched non-key predicate",
            d.label
        );
        d.commit();
        assert_eq!(d.read(READ_ALL), vec![(1, 1), (2, 2)], "{}: post-COMMIT", d.label);
    }
}

/// ROLLBACK must still discard everything, including the UPDATE that now
/// actually matched.
#[test]
fn rollback_discards_the_own_insert_and_its_update() {
    for (conn, family) in DRIVERS {
        let d = staged(PK_DDL, conn, family);
        assert_eq!(
            d.dml(
                "UPDATE t SET n = 99 WHERE id = 1",
                "UPDATE t SET n = 99 WHERE id = $1",
                &[Value::Int4(1)]
            ),
            1,
            "{}: UPDATE of an own insert",
            d.label
        );
        d.rollback();
        assert_eq!(
            d.read(READ_ALL),
            Vec::<(i64, i64)>::new(),
            "{}: ROLLBACK must discard the staged insert AND the update applied to it",
            d.label
        );
    }
}

// ---------------------------------------------------------------------------
// ISOLATION — the direction the fix must NOT trade away
// ---------------------------------------------------------------------------

/// Read-your-own-writes must come from the transaction's own write set, never
/// from publishing the staged rows where anyone else can see them. A second
/// connection must observe nothing until COMMIT.
#[test]
fn a_second_session_never_sees_the_uncommitted_rows() {
    for family in [Family::Text, Family::Params] {
        let d = Driver::open(Conn::Session, family);
        let observer = d.db.create_wire_session("observer").expect("observer session");
        d.setup(PK_DDL);
        d.setup("INSERT INTO t (id, n) VALUES (1, 1)");

        let observed = |context: &str| -> Vec<(i64, i64)> {
            let (rows, _cols) =
                d.db.query_with_columns_for_session(observer, READ_ALL)
                    .unwrap_or_else(|e| panic!("observer read ({context}): {e}"));
            pairs(&rows)
        };

        d.begin();
        d.insert("t", 2, 2);
        assert_eq!(
            d.dml(
                "UPDATE t SET n = 99 WHERE id = 2",
                "UPDATE t SET n = 99 WHERE id = $1",
                &[Value::Int4(2)]
            ),
            1,
            "{}: the writer must see its own insert",
            d.label
        );
        assert_eq!(
            d.dml(
                "DELETE FROM t WHERE id = 1",
                "DELETE FROM t WHERE id = $1",
                &[Value::Int4(1)]
            ),
            1,
            "{}: the writer deletes the committed row",
            d.label
        );

        assert_eq!(
            observed("mid-transaction"),
            vec![(1, 1)],
            "{}: a second session must see the pre-transaction state — not the staged \
             insert, not its update, and not the staged delete",
            d.label
        );

        d.commit();
        assert_eq!(
            observed("post-COMMIT"),
            vec![(2, 99)],
            "{}: and everything at once after COMMIT",
            d.label
        );

        d.db.destroy_session(observer).expect("destroy observer");
    }
}
