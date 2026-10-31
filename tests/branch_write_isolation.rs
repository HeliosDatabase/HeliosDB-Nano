//! sprinter 6bc4a4040eb7, phase B2 — writes and constraints on a branch.
//!
//! Plan: `docs/plans/BRANCH_ISOLATION.md` (D2, D3). Spec:
//! `docs/plans/specs/B2_branch_writes.md`.
//!
//! Every scenario runs on BOTH DML executor families — the text family
//! (`EmbeddedDatabase::execute` with literals) and the params family
//! (`execute_params` with bound `$n`, the extended protocol's funnel) — and
//! ends the same way: back on main, main's rows of every table it touched are
//! byte-for-byte what they were before the branch existed (row count plus an
//! ordered checksum), once with the branch alive and once after `DROP BRANCH`.
//! No scenario can pass while writing main.
//!
//! On the pre-B2 tree each scenario fails: PRIMARY KEY / UNIQUE were checked
//! against main's ART only (or not at all on the literal fast path), `ON
//! CONFLICT DO UPDATE` could not find an inherited row (text) or wrote main
//! (params), a transaction on a branch was refused (0A000), the params
//! in-transaction DELETE and the FK actions left inherited rows visible,
//! `TRUNCATE` on a branch deleted MAIN's rows, branch `INSERT … SELECT`
//! bypassed the transaction, kNN on a branch was answered from main's HNSW,
//! and a parent's tombstone hid a child branch's own row.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Tuple, Value};
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

// ---------------------------------------------------------------------------
// The two executor families
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// `execute` / `query` with the parameters inlined as SQL literals.
    Text,
    /// `execute_params` / `query_params` with bound `$n`.
    Params,
}

const FAMILIES: [Family; 2] = [Family::Text, Family::Params];

fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::Boolean(v) => v.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => panic!("no SQL literal for {other:?}"),
    }
}

/// `sql` with each `$n` replaced by the literal of `params[n - 1]` — the text
/// family's spelling of the same statement.
fn inline(sql: &str, params: &[Value]) -> String {
    let mut out = sql.to_string();
    for (index, value) in params.iter().enumerate().rev() {
        out = out.replace(&format!("${}", index + 1), &literal(value));
    }
    out
}

fn exec(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(&inline(sql, params)),
        Family::Params => db.execute_params(sql, params),
    }
}

fn ok(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> u64 {
    exec(db, family, sql, params).unwrap_or_else(|e| panic!("[{family:?}] {sql} {params:?}: {e}"))
}

fn rows(db: &EmbeddedDatabase, family: Family, sql: &str, params: &[Value]) -> Vec<Tuple> {
    match family {
        Family::Text => db.query(&inline(sql, params), &[]),
        Family::Params => db.query_params(sql, params),
    }
    .unwrap_or_else(|e| panic!("[{family:?}] {sql} {params:?}: {e}"))
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        other => panic!("expected text, got {other:?}"),
    }
}

/// First column of every row, as integers.
fn ids(rows: &[Tuple]) -> Vec<i64> {
    rows.iter().map(|row| int(row.values.first())).collect()
}

fn i(v: i32) -> Value {
    Value::Int4(v)
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// SQLSTATE 23505's message shape (the PostgreSQL wire maps exactly these to
/// 23505 — `protocol::postgres::handler::sqlstate_for_error`).
fn is_unique_violation(err: &Error) -> bool {
    match err {
        Error::ConstraintViolation(message) => {
            let lower = message.to_ascii_lowercase();
            !lower.contains("foreign key") && (lower.contains("duplicate key") || lower.contains("unique constraint"))
        }
        _ => false,
    }
}

/// SQLSTATE 23503's message shape.
fn is_fk_violation(err: &Error) -> bool {
    match err {
        Error::ConstraintViolation(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("foreign key constraint") || lower.contains("foreign key violation")
        }
        _ => false,
    }
}

fn assert_unique_violation(result: heliosdb_nano::Result<u64>, family: Family, what: &str) {
    match result {
        Err(err) if is_unique_violation(&err) => {}
        Err(err) => panic!("[{family:?}] {what}: expected a 23505 unique violation, got: {err}"),
        Ok(n) => panic!("[{family:?}] {what}: expected a 23505 unique violation, the statement succeeded ({n})"),
    }
}

fn assert_fk_violation(result: heliosdb_nano::Result<u64>, family: Family, what: &str) {
    match result {
        Err(err) if is_fk_violation(&err) => {}
        Err(err) => panic!("[{family:?}] {what}: expected a 23503 FK violation, got: {err}"),
        Ok(n) => panic!("[{family:?}] {what}: expected a 23503 FK violation, the statement succeeded ({n})"),
    }
}

// ---------------------------------------------------------------------------
// Main must be untouched
// ---------------------------------------------------------------------------

/// FNV-1a over `bytes`, folded into `hash`.
fn fold(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// What MAIN holds in `table`: (row count, ordered checksum) of its raw
/// `data:{table}:` keys and stored values, read straight from RocksDB in key
/// order.
///
/// Deliberately NOT a SQL read: the result cache answers a repeated query
/// with identical text from its first execution, so a `SELECT` fingerprint
/// taken before and after the branch work could not change whatever the
/// branch did to main. Raw keys are what a branch write must never touch.
/// (`_order_by` is kept so the snapshot tables read naturally.)
fn main_state(db: &EmbeddedDatabase, table: &str, _order_by: &str) -> (usize, u64) {
    let prefix = format!("data:{table}:");
    let raw = db.storage.db();
    let mut read_opts = rocksdb::ReadOptions::default();
    read_opts.set_total_order_seek(true);
    let mut count = 0usize;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for item in raw.iterator_opt(
        rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
        read_opts,
    ) {
        let (key, value) = item.unwrap();
        if !key.starts_with(prefix.as_bytes()) {
            break;
        }
        count += 1;
        hash = fold(hash, &key);
        hash = fold(hash, b"=");
        hash = fold(hash, &value);
        hash = fold(hash, b"|");
    }
    (count, hash)
}

struct MainSnapshot(Vec<(&'static str, &'static str, (usize, u64))>);

fn snapshot_main(db: &EmbeddedDatabase, tables: &[(&'static str, &'static str)]) -> MainSnapshot {
    MainSnapshot(
        tables
            .iter()
            .map(|(table, order_by)| (*table, *order_by, main_state(db, table, order_by)))
            .collect(),
    )
}

fn assert_main_unchanged(db: &EmbeddedDatabase, snapshot: &MainSnapshot, family: Family, when: &str) {
    for (table, order_by, before) in &snapshot.0 {
        assert_eq!(
            &main_state(db, table, order_by),
            before,
            "[{family:?}] *** MAIN's `{table}` changed {when} ***"
        );
    }
}

/// Back to main; main unchanged with the branches alive; drop them (in the
/// given order — children first); main still unchanged.
fn finish(db: &EmbeddedDatabase, family: Family, branches: &[&str], snapshot: &MainSnapshot) {
    db.execute("USE BRANCH main").unwrap();
    assert_main_unchanged(db, snapshot, family, "while the branch is alive");
    for branch in branches {
        db.execute(&format!("DROP BRANCH {branch}")).unwrap();
    }
    assert_main_unchanged(db, snapshot, family, "after DROP BRANCH");
}

fn branch(db: &EmbeddedDatabase, name: &str) {
    db.execute(&format!("CREATE BRANCH {name} AS OF NOW")).unwrap();
    db.execute(&format!("USE BRANCH {name}")).unwrap();
}

// ---------------------------------------------------------------------------
// PRIMARY KEY / UNIQUE over the branch's view
// ---------------------------------------------------------------------------

fn u_row(id: i32, code: Option<&str>, a: Option<i32>, b: Option<i32>) -> Vec<Value> {
    vec![
        i(id),
        code.map_or(Value::Null, s),
        a.map_or(Value::Null, i),
        b.map_or(Value::Null, i),
    ]
}

fn check_pk_and_unique(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, code TEXT UNIQUE, a INT, b INT, UNIQUE (a, b))")
        .unwrap();
    db.execute("INSERT INTO u VALUES (1, 'c1', 1, 1), (2, 'c2', 1, 2)")
        .unwrap();
    let snapshot = snapshot_main(&db, &[("u", "id")]);
    branch(&db, "b");
    let ins = "INSERT INTO u VALUES ($1, $2, $3, $4)";

    assert_unique_violation(
        exec(&db, family, ins, &u_row(1, Some("x"), Some(9), Some(9))),
        family,
        "PRIMARY KEY duplicate of an inherited row",
    );
    assert_unique_violation(
        exec(&db, family, ins, &u_row(3, Some("c1"), Some(9), Some(9))),
        family,
        "UNIQUE duplicate of an inherited value",
    );
    assert_unique_violation(
        exec(&db, family, ins, &u_row(3, Some("c3"), Some(1), Some(1))),
        family,
        "composite UNIQUE duplicate of an inherited pair",
    );
    ok(&db, family, ins, &u_row(3, Some("c3"), Some(3), Some(3)));
    assert_unique_violation(
        exec(&db, family, ins, &u_row(3, Some("c4"), Some(4), Some(4))),
        family,
        "PRIMARY KEY duplicate among branch rows",
    );
    assert_unique_violation(
        exec(&db, family, ins, &u_row(4, Some("c3"), Some(4), Some(4))),
        family,
        "UNIQUE duplicate among branch rows",
    );
    assert_unique_violation(
        exec(&db, family, ins, &u_row(4, Some("c4"), Some(3), Some(3))),
        family,
        "composite UNIQUE duplicate among branch rows",
    );

    // A duplicate WITHIN one multi-row INSERT — and nothing of it lands.
    let mut two = u_row(5, Some("c5"), Some(5), Some(5));
    two.extend(u_row(5, Some("c6"), Some(6), Some(6)));
    assert_unique_violation(
        exec(
            &db,
            family,
            "INSERT INTO u VALUES ($1, $2, $3, $4), ($5, $6, $7, $8)",
            &two,
        ),
        family,
        "duplicate within one multi-row INSERT",
    );
    assert!(
        rows(&db, family, "SELECT id FROM u WHERE id = $1", &[i(5)]).is_empty(),
        "[{family:?}] a failed multi-row INSERT left a row on the branch"
    );

    // UPDATE onto a duplicate key.
    assert_unique_violation(
        exec(&db, family, "UPDATE u SET code = $1 WHERE id = $2", &[s("c2"), i(3)]),
        family,
        "UPDATE onto an inherited UNIQUE value",
    );
    assert_unique_violation(
        exec(&db, family, "UPDATE u SET id = $1 WHERE id = $2", &[i(2), i(3)]),
        family,
        "UPDATE onto an inherited PRIMARY KEY",
    );

    // A key the branch deleted — or updated away — is free on the branch.
    ok(&db, family, "DELETE FROM u WHERE id = $1", &[i(1)]);
    ok(&db, family, ins, &u_row(1, Some("c1"), Some(1), Some(1)));
    ok(
        &db,
        family,
        "UPDATE u SET code = $1 WHERE id = $2",
        &[s("c2-moved"), i(2)],
    );
    ok(&db, family, "UPDATE u SET code = $1 WHERE id = $2", &[s("c2"), i(3)]);

    // NULLs in a UNIQUE column (and in a composite) never conflict.
    ok(&db, family, ins, &u_row(6, None, None, Some(7)));
    ok(&db, family, ins, &u_row(7, None, None, Some(7)));

    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM u ORDER BY id", &[])),
        vec![1, 2, 3, 6, 7],
        "[{family:?}] the branch's rows"
    );
    let three = rows(&db, family, "SELECT code FROM u WHERE id = $1", &[i(3)]);
    assert_eq!(text(three[0].values.first()).as_deref(), Some("c2"));
    finish(&db, family, &["b"], &snapshot);
}

/// PRIMARY KEY and every UNIQUE rule (column, table-level composite) hold over
/// the rows the branch sees; NULLs never conflict; a key the branch freed is
/// free there.
#[test]
fn pk_and_unique_hold_on_a_branch() {
    for family in FAMILIES {
        check_pk_and_unique(family);
    }
}

fn check_text_primary_key(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE tp (k TEXT PRIMARY KEY, n INT)").unwrap();
    db.execute("INSERT INTO tp VALUES ('alpha', 1), ('alphabet', 2)")
        .unwrap();
    let snapshot = snapshot_main(&db, &[("tp", "k")]);
    branch(&db, "b");
    let ins = "INSERT INTO tp VALUES ($1, $2)";
    assert_unique_violation(exec(&db, family, ins, &[s("alpha"), i(9)]), family, "TEXT PK duplicate");
    // A prefix of an existing key is a different key (the prefix-key ART).
    ok(&db, family, ins, &[s("alph"), i(3)]);
    assert_unique_violation(
        exec(&db, family, ins, &[s("alph"), i(4)]),
        family,
        "TEXT PK duplicate among branch rows",
    );
    ok(&db, family, "DELETE FROM tp WHERE k = $1", &[s("alphabet")]);
    ok(&db, family, ins, &[s("alphabet"), i(5)]);
    let seen: Vec<(Option<String>, i64)> = rows(&db, family, "SELECT k, n FROM tp ORDER BY k", &[])
        .iter()
        .map(|row| (text(row.values.first()), int(row.values.get(1))))
        .collect();
    assert_eq!(
        seen,
        vec![
            (Some("alph".to_string()), 3),
            (Some("alpha".to_string()), 1),
            (Some("alphabet".to_string()), 5)
        ],
        "[{family:?}]"
    );
    finish(&db, family, &["b"], &snapshot);
}

/// A TEXT primary key (prefix-key ART): duplicates refused, prefixes distinct,
/// a deleted key reusable — on the branch.
#[test]
fn text_primary_key_on_a_branch() {
    for family in FAMILIES {
        check_text_primary_key(family);
    }
}

/// The exact shape the code-graph upsert sends (`DELETE … WHERE path = $1`
/// then `INSERT … VALUES ($1, …)` through `execute_params_returning`) on a
/// branch: the INSERT used to pre-check MAIN's ART and fail "Duplicate key".
#[test]
fn params_delete_then_reinsert_of_an_inherited_key_on_a_branch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, 'rust', $2)",
        &[s("a.rs"), s("main body")],
    )
    .unwrap();
    let snapshot = snapshot_main(&db, &[("src", "path")]);
    db.execute("CREATE BRANCH preview AS OF NOW").unwrap();
    db.switch_branch("preview").unwrap();
    db.execute_params_returning("DELETE FROM src WHERE path = $1", &[s("a.rs")])
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, 'rust', $2)",
        &[s("a.rs"), s("branch body")],
    )
    .expect("re-inserting a key the branch deleted");
    let seen = db
        .query_params("SELECT content FROM src WHERE path = $1", &[s("a.rs")])
        .unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(text(seen[0].values.first()).as_deref(), Some("branch body"));
    finish(&db, Family::Params, &["preview"], &snapshot);
}

// ---------------------------------------------------------------------------
// ON CONFLICT
// ---------------------------------------------------------------------------

fn oc_rows(db: &EmbeddedDatabase, family: Family) -> Vec<(i64, Option<String>, i64)> {
    rows(db, family, "SELECT id, v, n FROM oc ORDER BY id", &[])
        .iter()
        .map(|row| (int(row.values.first()), text(row.values.get(1)), int(row.values.get(2))))
        .collect()
}

fn check_on_conflict(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE oc (id INT PRIMARY KEY, v TEXT, n INT)")
        .unwrap();
    db.execute("INSERT INTO oc VALUES (1, 'm1', 10), (2, 'm2', 20)")
        .unwrap();
    let snapshot = snapshot_main(&db, &[("oc", "id")]);
    branch(&db, "b");
    let upsert = "INSERT INTO oc VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v, n = oc.n + 1";
    let skip = "INSERT INTO oc VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING";

    // DO UPDATE of an INHERITED row updates the branch only.
    ok(&db, family, upsert, &[i(1), s("new"), i(0)]);
    // DO NOTHING skips a conflict with an inherited row …
    assert_eq!(
        ok(&db, family, skip, &[i(2), s("zzz"), i(0)]),
        0,
        "[{family:?}] DO NOTHING skipped"
    );
    // … and inserts a row that conflicts with nothing.
    assert_eq!(
        ok(&db, family, skip, &[i(3), s("b3"), i(30)]),
        1,
        "[{family:?}] DO NOTHING inserted"
    );
    // DO UPDATE of a row only the branch holds.
    ok(&db, family, upsert, &[i(3), s("b3x"), i(0)]);
    // ON CONFLICT on a key the branch deleted is a plain insert.
    ok(&db, family, "DELETE FROM oc WHERE id = $1", &[i(2)]);
    ok(&db, family, upsert, &[i(2), s("re"), i(5)]);

    assert_eq!(
        oc_rows(&db, family),
        vec![
            (1, Some("new".to_string()), 11),
            (2, Some("re".to_string()), 5),
            (3, Some("b3x".to_string()), 31)
        ],
        "[{family:?}] the branch's rows after the upserts"
    );
    finish(&db, family, &["b"], &snapshot);
}

/// `INSERT … ON CONFLICT DO UPDATE / DO NOTHING` on a branch arbitrates over
/// the branch's view and writes the branch only.
#[test]
fn on_conflict_on_a_branch() {
    for family in FAMILIES {
        check_on_conflict(family);
    }
}

/// `RETURNING` (a params-family statement on every interface) returns the
/// BRANCH's values: DO UPDATE of an inherited row, UPDATE and DELETE.
#[test]
fn returning_on_a_branch_returns_the_branch_values() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE oc (id INT PRIMARY KEY, v TEXT, n INT)")
        .unwrap();
    db.execute("INSERT INTO oc VALUES (1, 'm1', 10), (2, 'm2', 20)")
        .unwrap();
    let snapshot = snapshot_main(&db, &[("oc", "id")]);
    branch(&db, "b");
    let (_, returned) = db
        .execute_params_returning(
            "INSERT INTO oc VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v, n = oc.n + 1 \
             RETURNING id, v, n",
            &[i(2), s("ret"), i(0)],
        )
        .unwrap();
    assert_eq!(returned.len(), 1);
    assert_eq!(text(returned[0].values.get(1)).as_deref(), Some("ret"));
    assert_eq!(int(returned[0].values.get(2)), 21);
    let (_, returned) = db
        .execute_params_returning("UPDATE oc SET n = n + $1 WHERE id = $2 RETURNING n", &[i(100), i(2)])
        .unwrap();
    assert_eq!(int(returned[0].values.first()), 121);
    let (_, returned) = db
        .execute_params_returning("DELETE FROM oc WHERE id = $1 RETURNING v", &[i(2)])
        .unwrap();
    assert_eq!(text(returned[0].values.first()).as_deref(), Some("ret"));
    assert!(db
        .query_params("SELECT id FROM oc WHERE id = $1", &[i(2)])
        .unwrap()
        .is_empty());
    finish(&db, Family::Params, &["b"], &snapshot);
}

// ---------------------------------------------------------------------------
// Explicit transactions on a branch
// ---------------------------------------------------------------------------

fn v_of(db: &EmbeddedDatabase, family: Family, id: i32) -> Option<String> {
    rows(db, family, "SELECT v FROM tx WHERE id = $1", &[i(id)])
        .first()
        .and_then(|row| text(row.values.first()))
}

fn check_explicit_transactions(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE tx (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO tx VALUES (1, 'm1'), (2, 'm2')").unwrap();
    let snapshot = snapshot_main(&db, &[("tx", "id")]);
    branch(&db, "b");

    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "INSERT INTO tx VALUES ($1, $2)", &[i(3), s("t3")]);
    assert_eq!(
        v_of(&db, family, 3).as_deref(),
        Some("t3"),
        "[{family:?}] INSERT then SELECT"
    );
    ok(&db, family, "UPDATE tx SET v = $1 WHERE id = $2", &[s("t1"), i(1)]);
    assert_eq!(
        v_of(&db, family, 1).as_deref(),
        Some("t1"),
        "[{family:?}] UPDATE then SELECT"
    );
    ok(&db, family, "DELETE FROM tx WHERE id = $1", &[i(2)]);
    assert_eq!(
        v_of(&db, family, 2),
        None,
        "[{family:?}] DELETE hides the inherited row in the txn"
    );
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM tx ORDER BY id", &[])),
        vec![1, 3],
        "[{family:?}] the transaction's view of the branch"
    );
    // A duplicate of a row staged by this transaction.
    assert_unique_violation(
        exec(&db, family, "INSERT INTO tx VALUES ($1, $2)", &[i(3), s("dup")]),
        family,
        "duplicate of a row staged in the same transaction",
    );
    ok(&db, family, "ROLLBACK", &[]);
    assert_eq!(
        v_of(&db, family, 1).as_deref(),
        Some("m1"),
        "[{family:?}] ROLLBACK discarded the UPDATE"
    );
    assert_eq!(
        v_of(&db, family, 2).as_deref(),
        Some("m2"),
        "[{family:?}] ROLLBACK discarded the DELETE"
    );
    assert_eq!(v_of(&db, family, 3), None, "[{family:?}] ROLLBACK discarded the INSERT");

    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "INSERT INTO tx VALUES ($1, $2)", &[i(4), s("t4")]);
    ok(&db, family, "UPDATE tx SET v = $1 WHERE id = $2", &[s("u1"), i(1)]);
    ok(&db, family, "DELETE FROM tx WHERE id = $1", &[i(2)]);
    ok(&db, family, "COMMIT", &[]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM tx ORDER BY id", &[])),
        vec![1, 4],
        "[{family:?}] COMMIT landed on the branch"
    );
    assert_eq!(v_of(&db, family, 1).as_deref(), Some("u1"));

    // Savepoints behave as on main.
    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "INSERT INTO tx VALUES ($1, $2)", &[i(5), s("s5")]);
    ok(&db, family, "SAVEPOINT sp", &[]);
    ok(&db, family, "INSERT INTO tx VALUES ($1, $2)", &[i(6), s("s6")]);
    ok(&db, family, "DELETE FROM tx WHERE id = $1", &[i(4)]);
    ok(&db, family, "ROLLBACK TO SAVEPOINT sp", &[]);
    ok(&db, family, "COMMIT", &[]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM tx ORDER BY id", &[])),
        vec![1, 4, 5],
        "[{family:?}] ROLLBACK TO SAVEPOINT on a branch"
    );

    // Main sees none of it (checked again, byte for byte, by `finish`).
    db.execute("USE BRANCH main").unwrap();
    assert_eq!(
        v_of(&db, family, 4),
        None,
        "[{family:?}] *** a branch COMMIT landed on main ***"
    );
    db.execute("USE BRANCH b").unwrap();
    finish(&db, family, &["b"], &snapshot);
}

/// A transaction on a branch: INSERT / UPDATE / DELETE then SELECT see the
/// transaction's own branch writes, ROLLBACK discards them, COMMIT lands them
/// on the branch only, savepoints work.
#[test]
fn explicit_transactions_on_a_branch() {
    for family in FAMILIES {
        check_explicit_transactions(family);
    }
}

// ---------------------------------------------------------------------------
// Foreign keys on a branch
// ---------------------------------------------------------------------------

fn check_foreign_keys(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("CREATE TABLE rparent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE rchild (id INT PRIMARY KEY, pid INT REFERENCES rparent(id) ON DELETE RESTRICT)")
        .unwrap();
    db.execute("CREATE TABLE sparent (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE schild (id INT PRIMARY KEY, pid INT REFERENCES sparent(id) ON DELETE SET NULL)")
        .unwrap();
    db.execute("INSERT INTO parent VALUES (1, 'p1'), (2, 'p2')").unwrap();
    db.execute("INSERT INTO child VALUES (10, 1), (11, 1), (12, 2)")
        .unwrap();
    db.execute("INSERT INTO rparent VALUES (1), (2), (3)").unwrap();
    db.execute("INSERT INTO rchild VALUES (20, 1)").unwrap();
    db.execute("INSERT INTO sparent VALUES (1)").unwrap();
    db.execute("INSERT INTO schild VALUES (30, 1)").unwrap();
    let snapshot = snapshot_main(
        &db,
        &[
            ("parent", "id"),
            ("child", "id"),
            ("rparent", "id"),
            ("rchild", "id"),
            ("sparent", "id"),
            ("schild", "id"),
        ],
    );
    branch(&db, "b");

    // DELETE inside a transaction (the params family's in-transaction DELETE
    // used to stage a delete of `bdata:` only, leaving an inherited row
    // visible) with an ON DELETE CASCADE child.
    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "DELETE FROM parent WHERE id = $1", &[i(2)]);
    assert!(
        rows(&db, family, "SELECT id FROM parent WHERE id = $1", &[i(2)]).is_empty(),
        "[{family:?}] the in-transaction DELETE hides the inherited parent"
    );
    assert!(
        rows(&db, family, "SELECT id FROM child WHERE id = $1", &[i(12)]).is_empty(),
        "[{family:?}] the cascade hides the inherited child inside the transaction"
    );
    ok(&db, family, "COMMIT", &[]);
    assert!(rows(&db, family, "SELECT id FROM parent WHERE id = $1", &[i(2)]).is_empty());

    // Autocommit cascade on a branch hides the children ON THE BRANCH only.
    ok(&db, family, "DELETE FROM parent WHERE id = $1", &[i(1)]);
    assert!(
        rows(&db, family, "SELECT id FROM child", &[]).is_empty(),
        "[{family:?}] ON DELETE CASCADE hides every child on the branch"
    );
    // A child must reference a parent visible ON THE BRANCH.
    assert_fk_violation(
        exec(&db, family, "INSERT INTO child VALUES ($1, $2)", &[i(13), i(1)]),
        family,
        "a child of a parent the branch deleted",
    );
    ok(&db, family, "INSERT INTO parent VALUES ($1, $2)", &[i(3), s("p3")]);
    ok(&db, family, "INSERT INTO child VALUES ($1, $2)", &[i(14), i(3)]);

    // RESTRICT acts on the branch's children.
    assert_fk_violation(
        exec(&db, family, "DELETE FROM rparent WHERE id = $1", &[i(1)]),
        family,
        "RESTRICT with an inherited child",
    );
    ok(&db, family, "DELETE FROM rchild WHERE id = $1", &[i(20)]);
    ok(&db, family, "DELETE FROM rparent WHERE id = $1", &[i(1)]);
    ok(&db, family, "INSERT INTO rchild VALUES ($1, $2)", &[i(21), i(2)]);
    assert_fk_violation(
        exec(&db, family, "DELETE FROM rparent WHERE id = $1", &[i(2)]),
        family,
        "RESTRICT with a branch-only child",
    );
    ok(&db, family, "DELETE FROM rparent WHERE id = $1", &[i(3)]);

    // SET NULL on the branch's children.
    ok(&db, family, "DELETE FROM sparent WHERE id = $1", &[i(1)]);
    let nulled = rows(&db, family, "SELECT pid FROM schild WHERE id = $1", &[i(30)]);
    assert_eq!(nulled.len(), 1, "[{family:?}] SET NULL keeps the child");
    assert!(
        matches!(nulled[0].values.first(), Some(Value::Null)),
        "[{family:?}] SET NULL on the branch, got {:?}",
        nulled[0].values
    );

    finish(&db, family, &["b"], &snapshot);
}

/// FOREIGN KEY on a branch: a child needs a parent the branch sees; RESTRICT,
/// CASCADE and SET NULL act on the branch's rows only; a DELETE inside a
/// transaction hides an inherited row.
#[test]
fn foreign_keys_on_a_branch() {
    for family in FAMILIES {
        check_foreign_keys(family);
    }
}

// ---------------------------------------------------------------------------
// TRUNCATE on a branch
// ---------------------------------------------------------------------------

fn check_truncate(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE tr (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO tr VALUES (1, 'm1'), (2, 'm2')").unwrap();
    let snapshot = snapshot_main(&db, &[("tr", "id")]);
    branch(&db, "b");
    ok(&db, family, "INSERT INTO tr VALUES ($1, $2)", &[i(3), s("b3")]);
    ok(&db, family, "TRUNCATE tr", &[]);
    assert!(
        rows(&db, family, "SELECT id FROM tr", &[]).is_empty(),
        "[{family:?}] TRUNCATE empties the table on the branch"
    );
    // Every key is free again on the branch.
    ok(&db, family, "INSERT INTO tr VALUES ($1, $2)", &[i(1), s("again")]);

    // Inside a transaction: ROLLBACK restores the branch's rows.
    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "TRUNCATE tr", &[]);
    assert!(
        rows(&db, family, "SELECT id FROM tr", &[]).is_empty(),
        "[{family:?}] TRUNCATE inside a transaction"
    );
    ok(&db, family, "ROLLBACK", &[]);
    let back = rows(&db, family, "SELECT id, v FROM tr ORDER BY id", &[]);
    assert_eq!(ids(&back), vec![1], "[{family:?}] ROLLBACK restored the branch's rows");
    assert_eq!(text(back[0].values.get(1)).as_deref(), Some("again"));
    finish(&db, family, &["b"], &snapshot);
    // B2 fix pass: main's PRIMARY KEY index survived the branch TRUNCATEs —
    // a TRUNCATE used to clear main's ART whenever it ran, so re-inserting a
    // key main still holds succeeded on main (a duplicate the raw-row
    // fingerprint above cannot see, because it is written afterwards).
    assert_unique_violation(
        exec(&db, family, "INSERT INTO tr VALUES ($1, $2)", &[i(1), s("main dup")]),
        family,
        "re-inserting a main key on main after a branch TRUNCATE",
    );
    assert_unique_violation(
        exec(&db, family, "INSERT INTO tr VALUES ($1, $2)", &[i(2), s("main dup")]),
        family,
        "re-inserting a main key on main after a branch TRUNCATE",
    );
}

/// `TRUNCATE` on a branch empties the table on the branch only (it used to
/// delete MAIN's rows), and a ROLLBACK restores it.
#[test]
fn truncate_on_a_branch_empties_the_branch_only() {
    for family in FAMILIES {
        check_truncate(family);
    }
}

// ---------------------------------------------------------------------------
// INSERT … SELECT on a branch
// ---------------------------------------------------------------------------

fn check_insert_select(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE src (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("CREATE TABLE dst (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO src VALUES (1, 's1'), (2, 's2'), (3, 's3'), (4, 's4')")
        .unwrap();
    db.execute("INSERT INTO dst VALUES (1, 'd1')").unwrap();
    let snapshot = snapshot_main(&db, &[("src", "id"), ("dst", "id")]);
    branch(&db, "b");
    // The source is read through the branch too.
    ok(&db, family, "INSERT INTO src VALUES ($1, $2)", &[i(5), s("s5")]);
    // (Literal bounds: the params family's INSERT … SELECT does not bind `$n`
    // inside its source query — on main either.)
    ok(&db, family, "INSERT INTO dst SELECT id, v FROM src WHERE id > 2", &[]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM dst ORDER BY id", &[])),
        vec![1, 3, 4, 5]
    );
    // A duplicate of an inherited row fails the whole statement.
    assert_unique_violation(
        exec(&db, family, "INSERT INTO dst SELECT id, v FROM src WHERE id <= 2", &[]),
        family,
        "INSERT … SELECT colliding with an inherited row",
    );
    assert!(
        rows(&db, family, "SELECT id FROM dst WHERE id = $1", &[i(2)]).is_empty(),
        "[{family:?}] a failed INSERT … SELECT left a row on the branch"
    );
    // Inside a transaction, ROLLBACK takes it back.
    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "DELETE FROM dst WHERE id = $1", &[i(1)]);
    ok(&db, family, "INSERT INTO dst SELECT id, v FROM src WHERE id <= 2", &[]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM dst ORDER BY id", &[])),
        vec![1, 2, 3, 4, 5]
    );
    ok(&db, family, "ROLLBACK", &[]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM dst ORDER BY id", &[])),
        vec![1, 3, 4, 5]
    );
    let one = rows(&db, family, "SELECT v FROM dst WHERE id = $1", &[i(1)]);
    assert_eq!(text(one[0].values.first()).as_deref(), Some("d1"));
    finish(&db, family, &["b"], &snapshot);
}

/// `INSERT … SELECT` on a branch lands on the branch only, checked over the
/// branch's view, atomic, and inside the caller's transaction.
#[test]
fn insert_select_on_a_branch() {
    for family in FAMILIES {
        check_insert_select(family);
    }
}

// ---------------------------------------------------------------------------
// Vector top-k on a branch
// ---------------------------------------------------------------------------

fn check_vector_top_k(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE vt (id INT PRIMARY KEY, emb VECTOR(3))")
        .unwrap();
    // Enough rows that main's kNN is served by the HNSW index rather than the
    // small-index exact fallback.
    let values: Vec<String> = (0..300).map(|n| format!("({}, '[{}, {}, 1]')", n, n, n + 1)).collect();
    db.execute(&format!("INSERT INTO vt VALUES {}", values.join(", ")))
        .unwrap();
    db.execute("CREATE INDEX vt_emb ON vt USING hnsw (emb)").unwrap();
    let snapshot = snapshot_main(&db, &[("vt", "id")]);
    branch(&db, "b");
    // The nearest row to [0, 1, 1] is inherited row 0: hide it, and add a
    // nearer-than-everything-else branch row.
    ok(&db, family, "DELETE FROM vt WHERE id = $1", &[i(0)]);
    ok(
        &db,
        family,
        "INSERT INTO vt VALUES ($1, $2)",
        &[i(1000), s("[0, 1, 1.05]")],
    );
    let top = ids(&rows(
        &db,
        family,
        "SELECT id FROM vt ORDER BY emb <-> '[0, 1, 1]' LIMIT 3",
        &[],
    ));
    assert_eq!(
        top.first(),
        Some(&1000),
        "[{family:?}] the branch's nearer row comes first: {top:?}"
    );
    assert!(
        !top.contains(&0),
        "[{family:?}] a row the branch deleted came back: {top:?}"
    );
    assert_eq!(top.len(), 3);
    finish(&db, family, &["b"], &snapshot);
}

/// kNN (`ORDER BY emb <-> $q LIMIT k`) on a branch returns the branch's rows —
/// main's HNSW index knows neither its inserts nor its deletes.
#[test]
fn vector_top_k_on_a_branch_returns_the_branch_rows() {
    for family in FAMILIES {
        check_vector_top_k(family);
    }
}

// ---------------------------------------------------------------------------
// Nearest level wins
// ---------------------------------------------------------------------------

fn check_child_own_row(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE nl (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO nl VALUES (1, 'm1'), (2, 'm2')").unwrap();
    let snapshot = snapshot_main(&db, &[("nl", "id")]);
    branch(&db, "p");
    ok(&db, family, "UPDATE nl SET v = $1 WHERE id = $2", &[s("p1"), i(1)]);
    // `CREATE BRANCH` without PARENT from a session on `p`: a child of `p`.
    branch(&db, "c");
    ok(&db, family, "UPDATE nl SET v = $1 WHERE id = $2", &[s("c1"), i(1)]);
    db.execute("USE BRANCH p").unwrap();
    ok(&db, family, "DELETE FROM nl WHERE id = $1", &[i(1)]);
    assert!(rows(&db, family, "SELECT id FROM nl WHERE id = $1", &[i(1)]).is_empty());
    db.execute("USE BRANCH c").unwrap();
    let seen = rows(&db, family, "SELECT v FROM nl WHERE id = $1", &[i(1)]);
    assert_eq!(
        seen.first().and_then(|row| text(row.values.first())).as_deref(),
        Some("c1"),
        "[{family:?}] the child's own row must survive its parent's tombstone"
    );
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM nl ORDER BY id", &[])),
        vec![1, 2]
    );
    finish(&db, family, &["c", "p"], &snapshot);
}

/// A child branch's own row is not hidden by a tombstone of the same row in
/// its parent (the pre-B2 merge applied every tombstone of the chain last).
#[test]
fn a_child_branch_row_is_not_hidden_by_its_parents_tombstone() {
    for family in FAMILIES {
        check_child_own_row(family);
    }
}

// ---------------------------------------------------------------------------
// Concurrent writers, row ids
// ---------------------------------------------------------------------------

/// Two sessions on the same branch: a key one of them has staged (not yet
/// committed) refuses the other's write of it (23505), the commit keeps it
/// refused, a rollback frees it.
#[test]
fn concurrent_writers_of_one_key_on_one_branch_collide() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ck (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO ck VALUES (1, 'm1')").unwrap();
    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    let snapshot = snapshot_main(&db, &[("ck", "id")]);
    let s1 = db.create_session("writer_1", IsolationLevel::ReadCommitted).unwrap();
    let s2 = db.create_session("writer_2", IsolationLevel::ReadCommitted).unwrap();
    db.execute_for_session(s1, "USE BRANCH b").unwrap();
    db.execute_for_session(s2, "USE BRANCH b").unwrap();

    db.begin_transaction_for_session(s1).unwrap();
    db.execute_for_session(s1, "INSERT INTO ck VALUES (5, 'from s1')")
        .unwrap();
    let raced = db.execute_for_session(s2, "INSERT INTO ck VALUES (5, 'from s2')");
    assert!(
        matches!(&raced, Err(e) if is_unique_violation(e)),
        "a key staged by another transaction on the branch must refuse: {raced:?}"
    );
    db.commit_transaction_for_session(s1).unwrap();
    let after = db.execute_for_session(s2, "INSERT INTO ck VALUES (5, 'again')");
    assert!(matches!(&after, Err(e) if is_unique_violation(e)), "{after:?}");

    db.begin_transaction_for_session(s1).unwrap();
    db.execute_for_session(s1, "INSERT INTO ck VALUES (6, 'tmp')").unwrap();
    let raced = db.execute_params_for_session(s2, "INSERT INTO ck VALUES ($1, $2)", &[i(6), s("from s2")]);
    assert!(matches!(&raced, Err(e) if is_unique_violation(e)), "{raced:?}");
    db.rollback_transaction_for_session(s1).unwrap();
    db.execute_params_for_session(s2, "INSERT INTO ck VALUES ($1, $2)", &[i(6), s("from s2")])
        .expect("the rolled-back claim is released");

    let (seen, _) = db
        .query_with_columns_for_session(s2, "SELECT id, v FROM ck ORDER BY id")
        .unwrap();
    assert_eq!(ids(&seen), vec![1, 5, 6]);
    assert_eq!(text(seen[1].values.get(1)).as_deref(), Some("from s1"));
    db.destroy_session(s1).unwrap();
    db.destroy_session(s2).unwrap();
    finish(&db, Family::Text, &["b"], &snapshot);
}

/// Row ids of `prefix…` keys, read straight from the store.
fn stored_row_ids(db: &EmbeddedDatabase, prefix: &str) -> Vec<u64> {
    let raw = db.storage.db();
    raw.iterator(rocksdb::IteratorMode::From(
        prefix.as_bytes(),
        rocksdb::Direction::Forward,
    ))
    .map_while(|item| item.ok())
    .take_while(|(key, _)| key.starts_with(prefix.as_bytes()))
    .filter_map(|(key, _)| std::str::from_utf8(&key[prefix.len()..]).ok()?.parse().ok())
    .collect()
}

/// A branch INSERT takes its row id from the table's shared, persistent
/// counter: a row main inserts later never reuses it.
#[test]
fn branch_and_main_row_ids_never_collide() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE ri (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO ri VALUES (1, 'm1')").unwrap();
        branch(&db, "b");
        ok(&db, family, "INSERT INTO ri VALUES ($1, $2)", &[i(100), s("branch")]);
        db.execute("USE BRANCH main").unwrap();
        ok(&db, family, "INSERT INTO ri VALUES ($1, $2)", &[i(200), s("main")]);
        let branch_id = db.storage.get_branch("b").unwrap().branch_id;
        let on_branch = stored_row_ids(&db, &format!("bdata:{branch_id}:ri:"));
        let on_main = stored_row_ids(&db, "data:ri:");
        assert_eq!(on_branch.len(), 1, "[{family:?}] {on_branch:?}");
        assert_eq!(on_main.len(), 2, "[{family:?}] {on_main:?}");
        assert!(
            on_branch.iter().all(|row_id| !on_main.contains(row_id)),
            "[{family:?}] a main row reused a branch row id: branch {on_branch:?}, main {on_main:?}"
        );
        db.execute("DROP BRANCH b").unwrap();
    }
}

// ---------------------------------------------------------------------------
// B2 fix pass: open main transactions, claims, savepoints, MVs, FK types, cost
// ---------------------------------------------------------------------------

/// `exec` on session `sid` (which may be inside its own transaction).
fn exec_for(
    db: &EmbeddedDatabase,
    sid: SessionId,
    family: Family,
    sql: &str,
    params: &[Value],
) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute_for_session(sid, &inline(sql, params)),
        Family::Params => db.execute_params_for_session(sid, sql, params),
    }
}

fn check_open_main_transaction(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE om (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("CREATE TABLE oc (id INT PRIMARY KEY, p INT REFERENCES om(id))")
        .unwrap();
    db.execute("CREATE TABLE ot (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO om VALUES (1, 'a'), (2, 'b')").unwrap();
    db.execute("INSERT INTO ot VALUES (1, 'x'), (2, 'y')").unwrap();
    let snapshot = snapshot_main(&db, &[("om", "id"), ("oc", "id"), ("ot", "id")]);
    branch(&db, "b");
    // A new session starts on main.
    let m = db.create_session("main_writer", IsolationLevel::ReadCommitted).unwrap();
    let ins_om = "INSERT INTO om VALUES ($1, $2)";
    let ins_ot = "INSERT INTO ot VALUES ($1, $2)";

    // An open main DELETE has taken row 1's entries out of main's ART — the
    // row is still committed, so the branch still inherits it.
    db.begin_transaction_for_session(m).unwrap();
    db.execute_for_session(m, "DELETE FROM om WHERE id = 1").unwrap();
    assert_unique_violation(
        exec(&db, family, ins_om, &[i(1), s("dup")]),
        family,
        "a PRIMARY KEY an open main DELETE has staged away",
    );
    assert_unique_violation(
        exec(&db, family, ins_om, &[i(10), s("a")]),
        family,
        "a UNIQUE value an open main DELETE has staged away",
    );
    // ...and the parent still satisfies a FOREIGN KEY on the branch.
    ok(&db, family, "INSERT INTO oc VALUES ($1, $2)", &[i(1), i(1)]);
    db.rollback_transaction_for_session(m).unwrap();

    // An open main UPDATE that moves the keys of row 2.
    db.begin_transaction_for_session(m).unwrap();
    db.execute_for_session(m, "UPDATE om SET id = 20, code = 'b2' WHERE id = 2")
        .unwrap();
    assert_unique_violation(
        exec(&db, family, ins_om, &[i(2), s("z")]),
        family,
        "a PRIMARY KEY an open main UPDATE moved away",
    );
    assert_unique_violation(
        exec(&db, family, ins_om, &[i(21), s("b")]),
        family,
        "a UNIQUE value an open main UPDATE moved away",
    );
    db.rollback_transaction_for_session(m).unwrap();

    // An open main TRUNCATE (the text family's in-transaction arm, which
    // stages the row deletes — it used to clear main's ART outright).
    db.begin_transaction_for_session(m).unwrap();
    db.execute_for_session(m, "TRUNCATE ot").unwrap();
    assert_unique_violation(
        exec(&db, family, ins_ot, &[i(1), s("dup")]),
        family,
        "a PRIMARY KEY an open main TRUNCATE has staged away",
    );
    assert_unique_violation(
        exec(&db, family, ins_ot, &[i(10), s("y")]),
        family,
        "a UNIQUE value an open main TRUNCATE has staged away",
    );
    db.rollback_transaction_for_session(m).unwrap();
    db.destroy_session(m).unwrap();

    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM oc WHERE id > 0 ORDER BY id", &[])),
        vec![1],
        "[{family:?}] the branch's child row"
    );
    finish(&db, family, &["b"], &snapshot);

    // On main: every rolled-back transaction left main's indexes as they
    // were. The TRUNCATE in particular used to leave main's trees EMPTY after
    // its ROLLBACK, so main then accepted duplicates of its own rows.
    for (sql, params, what) in [
        (
            ins_ot,
            [i(1), s("main dup")],
            "main PK after a rolled-back main TRUNCATE",
        ),
        (ins_ot, [i(10), s("x")], "main UNIQUE after a rolled-back main TRUNCATE"),
        (ins_om, [i(1), s("main dup")], "main PK after a rolled-back main DELETE"),
        (ins_om, [i(2), s("main dup")], "main PK after a rolled-back main UPDATE"),
        (ins_om, [i(10), s("b")], "main UNIQUE after a rolled-back main UPDATE"),
    ] {
        assert_unique_violation(exec(&db, family, sql, &params), family, what);
    }
}

/// sprinter 6bc4a4040eb7 (B2 fix pass, item 1): main's ART is maintained
/// eagerly, so an OPEN main transaction's DELETE, key-moving UPDATE or
/// TRUNCATE has already taken entries out of it while the rows are still
/// committed — and inherited by every branch. The branch's PRIMARY KEY /
/// UNIQUE / FOREIGN KEY checks must answer from main's committed rows then,
/// not from the stripped index. After the main ROLLBACKs, main's own index
/// still holds every key (the TRUNCATE's ROLLBACK used not to restore it).
#[test]
fn an_open_main_transaction_does_not_free_inherited_keys_on_a_branch() {
    for family in FAMILIES {
        check_open_main_transaction(family);
    }
}

/// B2 fix pass, item 4: `ON CONFLICT` on a branch sees a key another open
/// transaction on the branch has claimed (its uncommitted insert): `DO
/// NOTHING` skips the row instead of failing 23505; `DO UPDATE` gets the
/// answer main gives for the same race (main's eager ART entry names a row
/// that is not in storage yet: "existing row not found in storage"); after
/// the commit both act on the committed row.
#[test]
fn on_conflict_on_a_branch_sees_a_concurrent_transactions_key() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE cc (id INT PRIMARY KEY, code TEXT UNIQUE, v TEXT)")
            .unwrap();
        db.execute("INSERT INTO cc VALUES (1, 'm1', 'main')").unwrap();
        db.execute("CREATE BRANCH b AS OF NOW").unwrap();
        let snapshot = snapshot_main(&db, &[("cc", "id")]);
        let s1 = db.create_session("claimer", IsolationLevel::ReadCommitted).unwrap();
        let s2 = db.create_session("upserter", IsolationLevel::ReadCommitted).unwrap();
        db.execute_for_session(s1, "USE BRANCH b").unwrap();
        db.execute_for_session(s2, "USE BRANCH b").unwrap();
        let nothing_on_id = "INSERT INTO cc VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING";
        let nothing_any = "INSERT INTO cc VALUES ($1, $2, $3) ON CONFLICT DO NOTHING";
        let update_on_id = "INSERT INTO cc VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v";

        db.begin_transaction_for_session(s1).unwrap();
        db.execute_for_session(s1, "INSERT INTO cc VALUES (5, 'c5', 's1')")
            .unwrap();
        let skipped = exec_for(&db, s2, family, nothing_on_id, &[i(5), s("other"), s("s2")]);
        assert_eq!(
            skipped.as_ref().ok(),
            Some(&0),
            "[{family:?}] DO NOTHING on a PRIMARY KEY another transaction claimed: {skipped:?}"
        );
        let skipped = exec_for(&db, s2, family, nothing_any, &[i(6), s("c5"), s("s2")]);
        assert_eq!(
            skipped.as_ref().ok(),
            Some(&0),
            "[{family:?}] targetless DO NOTHING on a UNIQUE value another transaction claimed: {skipped:?}"
        );
        match exec_for(&db, s2, family, update_on_id, &[i(5), s("other"), s("s2")]) {
            Err(e) if e.to_string().contains("existing row not found in storage") => {}
            other => panic!(
                "[{family:?}] DO UPDATE on a claimed key: expected main's answer for an uncommitted conflicting \
                 row, got {other:?}"
            ),
        }
        db.commit_transaction_for_session(s1).unwrap();

        let skipped = exec_for(&db, s2, family, nothing_on_id, &[i(5), s("other"), s("s2")]);
        assert_eq!(
            skipped.as_ref().ok(),
            Some(&0),
            "[{family:?}] after the commit: {skipped:?}"
        );
        assert_eq!(
            exec_for(&db, s2, family, update_on_id, &[i(5), s("other"), s("s2")]).unwrap(),
            1,
            "[{family:?}] DO UPDATE on the committed row"
        );
        assert_eq!(
            exec_for(&db, s2, family, nothing_any, &[i(7), s("c7"), s("s2")]).unwrap(),
            1,
            "[{family:?}] DO NOTHING on a free key inserts"
        );
        let (seen, _) = db
            .query_with_columns_for_session(s2, "SELECT id, v FROM cc WHERE id >= 1 ORDER BY id")
            .unwrap();
        assert_eq!(ids(&seen), vec![1, 5, 7], "[{family:?}]");
        assert_eq!(text(seen[1].values.get(1)).as_deref(), Some("s2"), "[{family:?}]");
        db.destroy_session(s1).unwrap();
        db.destroy_session(s2).unwrap();
        finish(&db, family, &["b"], &snapshot);
    }
}

/// B2 fix pass, item 5: a key claimed after a SAVEPOINT is released by
/// `ROLLBACK TO` that savepoint — the claim used to survive until the whole
/// transaction ended, refusing another session's insert of a key nobody
/// holds. A key claimed BEFORE the savepoint stays held.
#[test]
fn rollback_to_savepoint_releases_the_keys_claimed_after_it() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE sp (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO sp VALUES (1, 'm1')").unwrap();
        db.execute("CREATE BRANCH b AS OF NOW").unwrap();
        let snapshot = snapshot_main(&db, &[("sp", "id")]);
        let s1 = db.create_session("savepointer", IsolationLevel::ReadCommitted).unwrap();
        let s2 = db.create_session("other", IsolationLevel::ReadCommitted).unwrap();
        db.execute_for_session(s1, "USE BRANCH b").unwrap();
        db.execute_for_session(s2, "USE BRANCH b").unwrap();
        let ins = "INSERT INTO sp VALUES ($1, $2)";

        db.begin_transaction_for_session(s1).unwrap();
        db.execute_for_session(s1, "INSERT INTO sp VALUES (2, 'kept')").unwrap();
        db.execute_for_session(s1, "SAVEPOINT before_seven").unwrap();
        db.execute_for_session(s1, "INSERT INTO sp VALUES (7, 's1')").unwrap();
        assert_unique_violation(
            exec_for(&db, s2, family, ins, &[i(7), s("s2")]),
            family,
            "a key claimed by an open transaction",
        );
        db.execute_for_session(s1, "ROLLBACK TO SAVEPOINT before_seven")
            .unwrap();
        exec_for(&db, s2, family, ins, &[i(7), s("s2")])
            .unwrap_or_else(|e| panic!("[{family:?}] the claim must roll back with its savepoint: {e}"));
        assert_unique_violation(
            exec_for(&db, s2, family, ins, &[i(2), s("s2")]),
            family,
            "a key claimed before the savepoint stays held",
        );
        db.commit_transaction_for_session(s1).unwrap();

        let (seen, _) = db
            .query_with_columns_for_session(s2, "SELECT id, v FROM sp WHERE id >= 1 ORDER BY id")
            .unwrap();
        assert_eq!(ids(&seen), vec![1, 2, 7], "[{family:?}]");
        assert_eq!(text(seen[2].values.get(1)).as_deref(), Some("s2"), "[{family:?}]");
        db.destroy_session(s1).unwrap();
        db.destroy_session(s2).unwrap();
        finish(&db, family, &["b"], &snapshot);
    }
}

/// B2 fix pass (coverage): an UPDATE on a branch claims the keys it moves a
/// row TO — another session can neither insert nor update onto them until it
/// ends — and its ROLLBACK frees them.
#[test]
fn an_update_on_a_branch_holds_the_keys_it_moves_to() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE uc (id INT PRIMARY KEY, code TEXT UNIQUE)")
            .unwrap();
        db.execute("INSERT INTO uc VALUES (1, 'a'), (2, 'b')").unwrap();
        db.execute("CREATE BRANCH b AS OF NOW").unwrap();
        let snapshot = snapshot_main(&db, &[("uc", "id")]);
        let s1 = db.create_session("mover", IsolationLevel::ReadCommitted).unwrap();
        let s2 = db.create_session("other", IsolationLevel::ReadCommitted).unwrap();
        db.execute_for_session(s1, "USE BRANCH b").unwrap();
        db.execute_for_session(s2, "USE BRANCH b").unwrap();

        db.begin_transaction_for_session(s1).unwrap();
        db.execute_for_session(s1, "UPDATE uc SET id = 50, code = 'z' WHERE id = 1")
            .unwrap();
        assert_unique_violation(
            exec_for(&db, s2, family, "INSERT INTO uc VALUES ($1, $2)", &[i(50), s("q")]),
            family,
            "INSERT onto a PRIMARY KEY an open UPDATE moved a row to",
        );
        assert_unique_violation(
            exec_for(&db, s2, family, "INSERT INTO uc VALUES ($1, $2)", &[i(51), s("z")]),
            family,
            "INSERT onto a UNIQUE value an open UPDATE moved a row to",
        );
        assert_unique_violation(
            exec_for(&db, s2, family, "UPDATE uc SET id = $1 WHERE id = $2", &[i(50), i(2)]),
            family,
            "UPDATE onto a PRIMARY KEY an open UPDATE moved a row to",
        );
        assert_unique_violation(
            exec_for(
                &db,
                s2,
                family,
                "UPDATE uc SET code = $1 WHERE id = $2",
                &[s("z"), i(2)],
            ),
            family,
            "UPDATE onto a UNIQUE value an open UPDATE moved a row to",
        );
        // The row's OLD key is still the committed row's on the branch.
        assert_unique_violation(
            exec_for(&db, s2, family, "INSERT INTO uc VALUES ($1, $2)", &[i(1), s("q")]),
            family,
            "INSERT onto the key an open UPDATE is moving away from",
        );
        db.rollback_transaction_for_session(s1).unwrap();
        exec_for(
            &db,
            s2,
            family,
            "UPDATE uc SET id = $1, code = $2 WHERE id = $3",
            &[i(50), s("z"), i(2)],
        )
        .unwrap_or_else(|e| panic!("[{family:?}] the rolled-back UPDATE's claims are released: {e}"));
        let (seen, _) = db
            .query_with_columns_for_session(s2, "SELECT id FROM uc WHERE id >= 1 ORDER BY id")
            .unwrap();
        assert_eq!(ids(&seen), vec![1, 50], "[{family:?}]");
        db.destroy_session(s1).unwrap();
        db.destroy_session(s2).unwrap();
        finish(&db, family, &["b"], &snapshot);
    }
}

/// B2 fix pass, item 2: `REFRESH MATERIALIZED VIEW` on a branch dropped,
/// purged and recreated the view's MAIN data table and refilled it on the
/// branch — main's view came back empty. Until B3 scopes materialized views,
/// REFRESH and CREATE MATERIALIZED VIEW are refused on a branch (0A000)
/// before anything is written, and main's view keeps its rows.
#[test]
fn materialized_views_are_refused_on_a_branch_and_mains_view_survives() {
    use heliosdb_nano::BRANCH_STATEMENT_UNSUPPORTED_MARKER;
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE mvs (id INT PRIMARY KEY, v INT)").unwrap();
        db.execute("INSERT INTO mvs VALUES (1, 10), (2, 20)").unwrap();
        db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM mvs")
            .unwrap();
        // `__mv_<view>` is the view's data table (`MaterializedViewCatalog::mv_data_table_name`).
        let snapshot = snapshot_main(&db, &[("mvs", "id"), ("__mv_mv", "id")]);
        assert_eq!(
            (snapshot.0[1].2).0,
            2,
            "[{family:?}] harness: main's view data table holds its two rows"
        );
        branch(&db, "b");
        ok(&db, family, "INSERT INTO mvs VALUES ($1, $2)", &[i(3), i(30)]);
        for sql in [
            "REFRESH MATERIALIZED VIEW mv",
            "REFRESH MATERIALIZED VIEW CONCURRENTLY mv",
            "CREATE MATERIALIZED VIEW mv2 AS SELECT id FROM mvs",
        ] {
            match exec(&db, family, sql, &[]) {
                Err(e) if e.to_string().contains(BRANCH_STATEMENT_UNSUPPORTED_MARKER) => {}
                // The params family plans through sqlparser's grammar, which
                // may not parse the materialized-view statements at all —
                // refused before anything runs either way; the text family
                // below must give the branch refusal itself.
                Err(_) if family == Family::Params => {}
                other => panic!("[{family:?}] {sql} on a branch must be refused (0A000), got {other:?}"),
            }
            match db.execute(sql) {
                Err(e) if e.to_string().contains(BRANCH_STATEMENT_UNSUPPORTED_MARKER) => {}
                other => panic!("{sql} on a branch must be refused (0A000), got {other:?}"),
            }
        }
        finish(&db, family, &["b"], &snapshot);
        // Main can still refresh its view.
        db.execute("REFRESH MATERIALIZED VIEW mv").unwrap();
        assert_eq!(
            main_state(&db, "__mv_mv", "id").0,
            2,
            "[{family:?}] main's view after a main REFRESH"
        );
    }
}

fn check_cross_type_fk_actions(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE xp (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("CREATE TABLE xc (id INT PRIMARY KEY, p INT REFERENCES xp(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("CREATE TABLE xn (id INT PRIMARY KEY, p INT REFERENCES xp(id) ON DELETE SET NULL)")
        .unwrap();
    db.execute("INSERT INTO xp VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    db.execute("INSERT INTO xc VALUES (10, 1), (20, 2), (30, 3)").unwrap();
    db.execute("INSERT INTO xn VALUES (10, 1), (20, 2), (30, 3)").unwrap();

    // On main.
    ok(&db, family, "DELETE FROM xp WHERE id = $1", &[Value::Int8(1)]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM xc ORDER BY id", &[])),
        vec![20, 30],
        "[{family:?}] ON DELETE CASCADE from an int8 parent to an int4 child, on main"
    );
    let nulled = rows(&db, family, "SELECT p FROM xn WHERE id = $1", &[i(10)]);
    assert_eq!(
        nulled[0].values.first(),
        Some(&Value::Null),
        "[{family:?}] ON DELETE SET NULL from an int8 parent to an int4 child, on main"
    );

    // On a branch.
    let snapshot = snapshot_main(&db, &[("xp", "id"), ("xc", "id"), ("xn", "id")]);
    branch(&db, "b");
    ok(&db, family, "DELETE FROM xp WHERE id = $1", &[Value::Int8(2)]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM xc WHERE id > 0 ORDER BY id", &[])),
        vec![30],
        "[{family:?}] ON DELETE CASCADE from an int8 parent to an int4 child, on a branch"
    );
    let nulled = rows(&db, family, "SELECT p, id FROM xn WHERE id = $1", &[i(20)]);
    assert_eq!(
        nulled[0].values.first(),
        Some(&Value::Null),
        "[{family:?}] ON DELETE SET NULL from an int8 parent to an int4 child, on a branch"
    );
    finish(&db, family, &["b"], &snapshot);
}

/// B2 fix pass, item 8: the FK actions compared the parent's key values, as
/// the parent stores them, with the child's — and `Value` equality is
/// type-exact, so an int8 parent never matched an int4 child: ON DELETE
/// CASCADE / SET NULL silently left the children behind, on main as on a
/// branch. Both now convert the parent's values to the child columns' types
/// first (as the RESTRICT / FK probe already did).
#[test]
fn fk_actions_convert_the_parents_values_to_the_childs_types() {
    for family in FAMILIES {
        check_cross_type_fk_actions(family);
    }
}

/// `count` single-row INSERTs into `table` (id, code) from id `start`.
fn timed_inserts(db: &EmbeddedDatabase, family: Family, table: &str, start: i32, count: i32) -> Duration {
    let sql = format!("INSERT INTO {table} VALUES ($1, $2)");
    let started = Instant::now();
    for id in start..start + count {
        ok(db, family, &sql, &[i(id), s(&format!("c{id}"))]);
    }
    started.elapsed()
}

/// (first 500, last 500) of 2,000 single-row INSERTs.
fn first_and_last_of_2000(db: &EmbeddedDatabase, family: Family, table: &str) -> (Duration, Duration) {
    let first = timed_inserts(db, family, table, 1, 500);
    let _ = timed_inserts(db, family, table, 501, 1000);
    let last = timed_inserts(db, family, table, 1501, 500);
    (first, last)
}

/// B2 fix pass, item 3: each constrained write on a branch used to rebuild
/// its view of the branch's rows — the committed overlay once per
/// statement, the transaction's staged rows once per write — so N
/// single-row INSERTs cost O(N²). The committed overlay is now cached across
/// statements (validated by the write generation of every level of the
/// chain) and the staged rows are indexed as they are written.
///
/// Measured as a RATIO — the last 500 of 2,000 INSERTs against the first
/// 500, which with the quadratic cost is several times slower — for
/// autocommit INSERTs (the committed overlay grows) and for INSERTs inside
/// one transaction (the staged rows grow). A loaded machine can stall any
/// window, so each family gets three attempts and must pass one.
#[test]
fn single_row_inserts_on_a_branch_cost_linear_time() {
    const MAX_RATIO: f64 = 3.0;
    for family in FAMILIES {
        let mut attempts = Vec::new();
        let passed = (0..3).any(|_| {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE la (id INT PRIMARY KEY, code TEXT UNIQUE)")
                .unwrap();
            db.execute("CREATE TABLE lt (id INT PRIMARY KEY, code TEXT UNIQUE)")
                .unwrap();
            db.execute("INSERT INTO la VALUES (0, 'main')").unwrap();
            db.execute("INSERT INTO lt VALUES (0, 'main')").unwrap();
            branch(&db, "b");

            let (auto_first, auto_last) = first_and_last_of_2000(&db, family, "la");
            ok(&db, family, "BEGIN", &[]);
            let (txn_first, txn_last) = first_and_last_of_2000(&db, family, "lt");
            ok(&db, family, "COMMIT", &[]);

            assert_eq!(
                rows(&db, family, "SELECT id FROM la WHERE id >= 0", &[]).len(),
                2001,
                "[{family:?}] every autocommit INSERT landed on the branch"
            );
            assert_eq!(
                rows(&db, family, "SELECT id FROM lt WHERE id >= 0", &[]).len(),
                2001,
                "[{family:?}] every in-transaction INSERT landed on the branch"
            );
            let auto_ratio = auto_last.as_secs_f64() / auto_first.as_secs_f64().max(1e-6);
            let txn_ratio = txn_last.as_secs_f64() / txn_first.as_secs_f64().max(1e-6);
            attempts.push(format!(
                "autocommit {auto_first:?} -> {auto_last:?} (x{auto_ratio:.2}), in a transaction {txn_first:?} -> \
                 {txn_last:?} (x{txn_ratio:.2})"
            ));
            auto_ratio <= MAX_RATIO && txn_ratio <= MAX_RATIO
        });
        assert!(
            passed,
            "[{family:?}] the last 500 of 2,000 single-row INSERTs on a branch must cost at most {MAX_RATIO}x the \
             first 500; attempts: {attempts:#?}"
        );
    }
}

// ---------------------------------------------------------------------------
// B2 follow-up: TRUNCATE inside a transaction, on main (and on a branch)
// ---------------------------------------------------------------------------

fn check_truncate_rolls_back_and_commits(family: Family) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE tt (id INT PRIMARY KEY, code TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO tt VALUES (1, 'a'), (2, 'b')").unwrap();
    let ins = "INSERT INTO tt VALUES ($1, $2)";

    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "TRUNCATE tt", &[]);
    assert!(
        rows(&db, family, "SELECT id FROM tt WHERE id > 0", &[]).is_empty(),
        "[{family:?}] TRUNCATE inside the transaction empties it for the transaction"
    );
    ok(&db, family, "ROLLBACK", &[]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM tt WHERE id >= 0 ORDER BY id", &[])),
        vec![1, 2],
        "[{family:?}] ROLLBACK restores every row a TRUNCATE in the transaction took"
    );
    assert_unique_violation(
        exec(&db, family, ins, &[i(1), s("x")]),
        family,
        "the PRIMARY KEY index entries come back with the rows",
    );
    assert_unique_violation(
        exec(&db, family, ins, &[i(9), s("a")]),
        family,
        "the UNIQUE index entries come back with the rows",
    );

    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, "TRUNCATE tt", &[]);
    ok(&db, family, "COMMIT", &[]);
    assert!(
        rows(&db, family, "SELECT id FROM tt WHERE id >= 1", &[]).is_empty(),
        "[{family:?}] COMMIT applies the TRUNCATE"
    );
    ok(&db, family, ins, &[i(1), s("a")]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM tt WHERE id >= -1 ORDER BY id", &[])),
        vec![1],
        "[{family:?}] the committed TRUNCATE freed every key"
    );
}

/// B2 follow-up, item 1: a TRUNCATE inside an explicit transaction is
/// transactional on BOTH families. The params family used to run
/// `ddl::handle_truncate`, which deletes `data:` at once: a ROLLBACK
/// restored nothing.
#[test]
fn truncate_in_a_transaction_rolls_back_and_commits_on_main() {
    for family in FAMILIES {
        check_truncate_rolls_back_and_commits(family);
    }
}

fn check_truncate_takes_the_transactions_own_rows(family: Family, on_branch: bool) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ti (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO ti VALUES (1, 'committed')").unwrap();
    let snapshot = snapshot_main(&db, &[("ti", "id")]);
    if on_branch {
        branch(&db, "b");
    }
    let ins = "INSERT INTO ti VALUES ($1, $2)";
    let at = if on_branch { "on a branch" } else { "on main" };

    // BEGIN; INSERT; TRUNCATE; SELECT — nothing, the transaction's own row
    // included; its key is free again; COMMIT keeps only what came after.
    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, ins, &[i(5), s("mine")]);
    ok(&db, family, "TRUNCATE ti", &[]);
    assert!(
        rows(&db, family, "SELECT id FROM ti WHERE id > 0", &[]).is_empty(),
        "[{family:?}] {at}: TRUNCATE removes the rows its own transaction inserted"
    );
    ok(&db, family, ins, &[i(5), s("again")]);
    ok(&db, family, "COMMIT", &[]);
    let seen = rows(&db, family, "SELECT id, v FROM ti WHERE id >= 0 ORDER BY id", &[]);
    assert_eq!(ids(&seen), vec![5], "[{family:?}] {at}: after COMMIT");
    assert_eq!(
        text(seen[0].values.get(1)).as_deref(),
        Some("again"),
        "[{family:?}] {at}"
    );

    // BEGIN; INSERT; TRUNCATE; COMMIT — the table is empty.
    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, ins, &[i(6), s("gone")]);
    ok(&db, family, "TRUNCATE ti", &[]);
    ok(&db, family, "COMMIT", &[]);
    assert!(
        rows(&db, family, "SELECT id FROM ti WHERE id >= 1", &[]).is_empty(),
        "[{family:?}] {at}: COMMIT leaves the table empty"
    );
    ok(&db, family, ins, &[i(6), s("free")]);

    // BEGIN; INSERT; TRUNCATE; ROLLBACK — back to before the transaction,
    // and the rolled-back row's key is free.
    ok(&db, family, "BEGIN", &[]);
    ok(&db, family, ins, &[i(7), s("rolled back")]);
    ok(&db, family, "TRUNCATE ti", &[]);
    ok(&db, family, "ROLLBACK", &[]);
    assert_eq!(
        ids(&rows(&db, family, "SELECT id FROM ti WHERE id >= -1 ORDER BY id", &[])),
        vec![6],
        "[{family:?}] {at}: after ROLLBACK"
    );
    ok(&db, family, ins, &[i(7), s("free")]);
    assert_unique_violation(
        exec(&db, family, ins, &[i(6), s("dup")]),
        family,
        "a row that survived the rolled-back TRUNCATE keeps its key",
    );

    if on_branch {
        finish(&db, family, &["b"], &snapshot);
    }
}

/// B2 follow-up, item 3: TRUNCATE inside a transaction takes the rows the
/// SAME transaction inserted before it. It used to read committed rows only
/// (`scan_table`), so `BEGIN; INSERT; TRUNCATE; SELECT` still returned the
/// row and COMMIT kept it. On main and on a branch (the branch view's
/// transaction-aware scan), both families.
#[test]
fn truncate_takes_the_rows_its_own_transaction_inserted() {
    for family in FAMILIES {
        check_truncate_takes_the_transactions_own_rows(family, false);
        check_truncate_takes_the_transactions_own_rows(family, true);
    }
}

/// Row count of a simple-query result.
fn simple_rows(messages: &[SimpleQueryMessage]) -> usize {
    messages
        .iter()
        .filter(|message| matches!(message, SimpleQueryMessage::Row(_)))
        .count()
}

/// B2 follow-up, items 1 and 3 over the PostgreSQL wire: the extended
/// protocol (the params family) and the simple protocol (the text family),
/// each in a session transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_in_a_wire_transaction_is_transactional() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE wt (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO wt VALUES (1, 'a'), (2, 'b')").unwrap();
    let s = serve(db).await;
    let (client, task) = connect(s.port).await;

    // Extended protocol: BEGIN; TRUNCATE; ROLLBACK restores rows and keys.
    client.batch_execute("BEGIN").await.unwrap();
    client.execute("TRUNCATE wt", &[]).await.unwrap();
    assert!(client
        .query("SELECT id FROM wt WHERE id > 0", &[])
        .await
        .unwrap()
        .is_empty());
    client.batch_execute("ROLLBACK").await.unwrap();
    let back = client
        .query("SELECT id FROM wt WHERE id >= 0 ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(
        back.iter().map(|row| row.get::<_, i32>(0)).collect::<Vec<_>>(),
        vec![1, 2],
        "extended protocol: ROLLBACK restores the TRUNCATEd rows"
    );
    let err = client
        .execute("INSERT INTO wt VALUES ($1, $2)", &[&1i32, &"dup"])
        .await
        .err()
        .expect("the PRIMARY KEY entry came back with the row");
    assert_eq!(sqlstate(&err), "23505");

    // Simple protocol: the same.
    client.batch_execute("BEGIN; TRUNCATE wt; ROLLBACK").await.unwrap();
    let back = client.simple_query("SELECT id FROM wt WHERE id >= -1").await.unwrap();
    assert_eq!(simple_rows(&back), 2, "simple protocol: ROLLBACK restores the rows");

    // Extended protocol: the transaction's own row goes with the TRUNCATE.
    client.batch_execute("BEGIN").await.unwrap();
    client
        .execute("INSERT INTO wt VALUES ($1, $2)", &[&7i32, &"mine"])
        .await
        .unwrap();
    client.execute("TRUNCATE wt", &[]).await.unwrap();
    assert!(
        client
            .query("SELECT id FROM wt WHERE id > -2", &[])
            .await
            .unwrap()
            .is_empty(),
        "extended protocol: TRUNCATE takes the transaction's own row"
    );
    client.batch_execute("COMMIT").await.unwrap();
    assert!(client
        .query("SELECT id FROM wt WHERE id > -3", &[])
        .await
        .unwrap()
        .is_empty());

    // Simple protocol: the same.
    client.batch_execute("INSERT INTO wt VALUES (1, 'a')").await.unwrap();
    client
        .batch_execute("BEGIN; INSERT INTO wt VALUES (8, 'mine'); TRUNCATE wt")
        .await
        .unwrap();
    let inside = client.simple_query("SELECT id FROM wt WHERE id > -4").await.unwrap();
    assert_eq!(
        simple_rows(&inside),
        0,
        "simple protocol: TRUNCATE takes the transaction's own row"
    );
    client.batch_execute("COMMIT").await.unwrap();
    let after = client.simple_query("SELECT id FROM wt WHERE id > -5").await.unwrap();
    assert_eq!(simple_rows(&after), 0, "simple protocol: COMMIT leaves the table empty");
    drop(client);
    task.abort();
}

/// B2 follow-up, item 4: an AUTOCOMMIT statement's eager index removals are
/// counted by the uncommitted-removal census for as long as its transaction
/// lives — a branch probe and main's miss-based fast paths trust main's index
/// only while that census is clear, and a multi-row DELETE can hold its
/// removals for a long statement and still roll them back. A plain staged
/// write (an INSERT) takes no slot, so an insert workload never turns the
/// COUNT fast paths off.
#[test]
fn an_autocommit_statements_index_removals_are_counted_while_it_runs() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE ac (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO ac VALUES (1, 'a')").unwrap();
    let census = db.storage.write_census();

    let insert_only = db.storage.begin_autocommit_transaction().unwrap();
    insert_only.put(b"data:ac:99".to_vec(), b"row".to_vec()).unwrap();
    assert!(
        !db.storage.has_uncommitted_writes(),
        "a statement that only adds rows takes no census slot"
    );
    drop(insert_only);

    let deleting = db.storage.begin_autocommit_transaction().unwrap();
    deleting.delete(b"data:ac:1".to_vec()).unwrap();
    assert!(
        db.storage.has_uncommitted_index_removals_for_table("ac"),
        "a statement's staged DELETE is counted while it runs"
    );
    assert!(!db.storage.has_uncommitted_index_removals_for_table("other"));
    drop(deleting);
    assert!(!db.storage.has_uncommitted_index_removals_for_table("ac"));

    let moving = db.storage.begin_autocommit_transaction().unwrap();
    moving.note_index_key_removal("ac");
    assert!(
        db.storage.has_uncommitted_index_removals_for_table("ac"),
        "a statement's key-moving UPDATE is counted while it runs"
    );
    drop(moving);
    assert_eq!(census.uncommitted_write_transactions(), 0, "every slot is released");

    // And the statements themselves still work, end to end.
    db.execute("DELETE FROM ac WHERE id = 1").unwrap();
    db.execute_params("INSERT INTO ac VALUES ($1, $2)", &[i(1), s("b")])
        .unwrap();
    assert_eq!(census.uncommitted_write_transactions(), 0);
}

// ---------------------------------------------------------------------------
// The PostgreSQL wire: SQLSTATEs and COPY
// ---------------------------------------------------------------------------

struct Server {
    db: Arc<EmbeddedDatabase>,
    port: u16,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

async fn serve(db: EmbeddedDatabase) -> Server {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let db = Arc::new(db);
    let server = PgServer::new(
        PgServerConfig::with_address(addr).with_max_connections(16),
        Arc::clone(&db),
    )
    .unwrap();
    let listener = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    Server {
        db,
        port: addr.port(),
        listener,
    }
}

async fn connect(port: u16) -> (Client, tokio::task::JoinHandle<()>) {
    let cs = format!("host=127.0.0.1 port={port} user=postgres dbname=postgres");
    let (client, conn) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
    let task = tokio::spawn(async move { conn.await.map(|_| ()).unwrap_or(()) });
    (client, task)
}

fn sqlstate(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.code().code().to_string())
        .unwrap_or_else(|| format!("<not a DbError: {e}>"))
}

/// Over the extended protocol: a duplicate on a branch is SQLSTATE 23505, a
/// missing FK parent 23503, a key the branch freed is free, and an `ON
/// CONFLICT … RETURNING` returns the branch's row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extended_protocol_sqlstates_on_a_branch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE w (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("CREATE TABLE wc (id INT PRIMARY KEY, wid INT REFERENCES w(id))")
        .unwrap();
    db.execute("INSERT INTO w VALUES (1, 'm1')").unwrap();
    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    let snapshot = snapshot_main(&db, &[("w", "id"), ("wc", "id")]);
    let s = serve(db).await;
    let (client, task) = connect(s.port).await;
    client.simple_query("USE BRANCH b").await.unwrap();

    let err = client
        .execute("INSERT INTO w VALUES ($1, $2)", &[&1i32, &"dup"])
        .await
        .err()
        .expect("a duplicate of an inherited key");
    assert_eq!(sqlstate(&err), "23505");
    client.execute("DELETE FROM w WHERE id = $1", &[&1i32]).await.unwrap();
    let err = client
        .execute("INSERT INTO wc VALUES ($1, $2)", &[&10i32, &1i32])
        .await
        .err()
        .expect("a child of a parent the branch deleted");
    assert_eq!(sqlstate(&err), "23503");
    client
        .execute("INSERT INTO w VALUES ($1, $2)", &[&1i32, &"again"])
        .await
        .expect("a key the branch freed");
    let returned = client
        .query(
            "INSERT INTO w VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v RETURNING v",
            &[&1i32, &"upserted"],
        )
        .await
        .unwrap();
    assert_eq!(returned[0].get::<_, String>(0), "upserted");
    drop(client);
    task.abort();

    s.db.execute("USE BRANCH main").unwrap();
    assert_main_unchanged(
        &s.db,
        &snapshot,
        Family::Params,
        "after the wire session's branch writes",
    );
    s.db.execute("DROP BRANCH b").unwrap();
    assert_main_unchanged(&s.db, &snapshot, Family::Params, "after DROP BRANCH");
}

mod raw_wire {
    use bytes::{BufMut, BytesMut};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn put_cstr(buf: &mut BytesMut, value: &str) {
        buf.extend_from_slice(value.as_bytes());
        buf.put_u8(0);
    }

    fn message(tag: u8, body: &[u8]) -> BytesMut {
        let mut msg = BytesMut::new();
        msg.put_u8(tag);
        msg.put_i32((body.len() + 4) as i32);
        msg.extend_from_slice(body);
        msg
    }

    async fn read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).await.unwrap();
        let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        stream.read_exact(&mut body).await.unwrap();
        (header[0], body)
    }

    pub async fn connect(port: u16) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut body = BytesMut::new();
        body.put_i32(196_608);
        put_cstr(&mut body, "user");
        put_cstr(&mut body, "postgres");
        put_cstr(&mut body, "database");
        put_cstr(&mut body, "postgres");
        body.put_u8(0);
        let mut startup = BytesMut::new();
        startup.put_i32((body.len() + 4) as i32);
        startup.extend_from_slice(&body);
        stream.write_all(&startup).await.unwrap();
        while read_frame(&mut stream).await.0 != b'Z' {}
        stream
    }

    /// Run `sql` as one `Q`; a CopyInResponse is answered with `copy_rows`
    /// and CopyDone. The ErrorResponse's message, if any.
    pub async fn query(stream: &mut TcpStream, sql: &str, copy_rows: &[&str]) -> Option<String> {
        let mut body = BytesMut::new();
        put_cstr(&mut body, sql);
        stream.write_all(&message(b'Q', &body)).await.unwrap();
        let mut error = None;
        loop {
            let (tag, body) = read_frame(stream).await;
            match tag {
                b'G' => {
                    let mut out = BytesMut::new();
                    for row in copy_rows {
                        out.extend_from_slice(&message(b'd', row.as_bytes()));
                    }
                    out.extend_from_slice(&message(b'c', &[]));
                    stream.write_all(&out).await.unwrap();
                }
                b'E' => error = Some(String::from_utf8_lossy(&body).to_string()),
                b'Z' => return error,
                _ => {}
            }
        }
    }
}

/// `COPY … FROM STDIN` on a branch lands on the branch only, is checked over
/// the branch's view, and is atomic: one duplicate (of an inherited row, or of
/// another row of the same COPY) takes the whole COPY back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_from_on_a_branch_is_atomic_and_branch_only() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE cp (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO cp VALUES (1, 'm1')").unwrap();
    db.execute("CREATE BRANCH b AS OF NOW").unwrap();
    let snapshot = snapshot_main(&db, &[("cp", "id")]);
    let s = serve(db).await;
    let mut wire = raw_wire::connect(s.port).await;
    assert_eq!(raw_wire::query(&mut wire, "USE BRANCH b", &[]).await, None);
    assert_eq!(
        raw_wire::query(&mut wire, "COPY cp FROM STDIN", &["2\tc2\n", "3\tc3\n"]).await,
        None,
        "COPY FROM on a branch"
    );
    // B2 fix pass: the COPY handler applies rows in 500-row chunks, so a
    // 2-row COPY proved nothing about atomicity ACROSS chunks. These are
    // 1,200 good rows with the duplicate placed in the THIRD chunk: two whole
    // chunks were applied before it, and must be taken back with it.
    let late_dup = |dup: &str| -> Vec<String> {
        let mut lines: Vec<String> = (100..1300).map(|id| format!("{id}\tc{id}\n")).collect();
        lines.insert(1100, dup.to_string());
        lines
    };
    let inherited_dup = late_dup("1\tdup\n");
    let refs: Vec<&str> = inherited_dup.iter().map(String::as_str).collect();
    let failed = raw_wire::query(&mut wire, "COPY cp FROM STDIN", &refs).await;
    assert!(
        failed.is_some(),
        "a COPY duplicating an inherited key in a later chunk must fail"
    );
    let own_dup = late_dup("150\tdup\n");
    let refs: Vec<&str> = own_dup.iter().map(String::as_str).collect();
    let failed = raw_wire::query(&mut wire, "COPY cp FROM STDIN", &refs).await;
    assert!(
        failed.is_some(),
        "a COPY duplicating its own first-chunk row in a later chunk must fail"
    );

    let reader = s.db.create_session("reader", IsolationLevel::ReadCommitted).unwrap();
    s.db.execute_for_session(reader, "USE BRANCH b").unwrap();
    let (seen, _) =
        s.db.query_with_columns_for_session(reader, "SELECT id FROM cp ORDER BY id")
            .unwrap();
    assert_eq!(
        ids(&seen),
        vec![1, 2, 3],
        "only the successful COPY landed — the failed ones took back their earlier chunks"
    );
    s.db.destroy_session(reader).unwrap();

    s.db.execute("USE BRANCH main").unwrap();
    assert_main_unchanged(&s.db, &snapshot, Family::Text, "after COPY on the branch");
    s.db.execute("DROP BRANCH b").unwrap();
    assert_main_unchanged(&s.db, &snapshot, Family::Text, "after DROP BRANCH");
}
