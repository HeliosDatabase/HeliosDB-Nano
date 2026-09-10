//! GH#25 — BYTEA fidelity through the ENGINE, on both executor families.
//!
//! The user-visible half of #25 is a wire-protocol defect (see the wire tests
//! appended to `src/protocol/postgres/wire_tests.rs`): the RowDescription type
//! OID for a bytea column is guessed from the runtime value of row 0 on the
//! simple-query path, and an aliased `RETURNING` of a bytea column is described
//! as text while its DataRow carries raw bytea binary bytes — which is how
//! 4,633 bytes reach a node-pg client as 4,516 UTF-8 code points.
//!
//! This file pins the layer UNDERNEATH that: whatever the wire does, a >4 KiB
//! bytea containing 0x00, 0x5c (backslash), 0x27 (single quote) and high bytes
//! must survive INSERT → storage → SELECT and INSERT → RETURNING byte for byte,
//! through BOTH executor families —
//!   * the TEXT family: `db.execute()` → `execute_in_transaction_inner`
//!     (psql simple query, MySQL wire, embedded);
//!   * the PARAMS family: `db.execute_params_returning()` / `db.query_params()`
//!     → `execute_plan_with_params_inner` (the PostgreSQL EXTENDED protocol
//!     every real driver uses, plus REST/BaaS).
//! A fix in one says nothing about the other, and the wire fix must not be
//! allowed to paper over an engine-level truncation.
//!
//! Expected on the current tree: every assertion here PASSES. That is the
//! point — it localises #25 to the protocol layer and becomes the permanent
//! floor under the wire fix.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

/// The reporter's 4,633-byte payload, carrying every byte class that any
/// encoder in the path can trip over: 0x00 (NUL), 0x5c (backslash — the byte
/// escape-format un-escaping silently drops), 0x27 (single quote — breaks
/// literal splicing), 0x22, 0x0a, and the high bytes that decide whether a
/// UTF-8 decode SHORTENS the value (4,633 raw bytes decode to ~4,516 code
/// points — the number in the bug report).
fn blob() -> Vec<u8> {
    const PATTERN: [u8; 10] = [0x00, 0x5c, 0x27, 0x22, 0x0a, 0x7f, 0x80, 0xa5, 0xc3, 0xff];
    let mut out = Vec::with_capacity(4633);
    while out.len() < 4633 {
        out.push(PATTERN[out.len() % PATTERN.len()]);
    }
    out
}

fn hex_literal(raw: &[u8]) -> String {
    let mut s = String::with_capacity(2 + raw.len() * 2);
    s.push_str("\\x");
    for b in raw {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn bytes_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Bytes(b) => b.clone(),
        other => panic!("expected Value::Bytes, got {other:?}"),
    }
}

/// POSITIVE CONTROL — the harness itself.
///
/// A five-byte bytea containing the backslash byte round-trips through the text
/// family. This passes before and after any fix; if it ever fails, the test
/// file (or the `\x` literal syntax) is what broke, not the bug under test.
#[test]
fn control_short_bytea_round_trips_through_the_text_family() {
    let db = mem_db();
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, b BYTEA)").unwrap();
    db.execute("INSERT INTO c VALUES (1, '\\x5a5b5c5d5e')").unwrap();
    let rows = db.query("SELECT b FROM c WHERE id = 1", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        bytes_of(rows[0].values.first().unwrap()),
        vec![0x5a, 0x5b, 0x5c, 0x5d, 0x5e],
        "the 0x5c byte must survive — escape-format un-escaping is what eats it"
    );
}

/// TEXT family: a 4,633-byte bytea written as a `\x…` literal must read back
/// byte for byte, with its length unchanged (not 4,516, which is what a UTF-8
/// decode of those bytes would leave).
#[test]
fn large_bytea_round_trips_through_the_text_family() {
    let db = mem_db();
    let payload = blob();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, b BYTEA)").unwrap();
    db.execute(&format!("INSERT INTO t VALUES (1, '{}')", hex_literal(&payload)))
        .unwrap();

    let rows = db.query("SELECT b FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    let got = bytes_of(rows[0].values.first().unwrap());
    assert_eq!(got.len(), 4633, "length must be exact, not a UTF-8 code-point count");
    assert_eq!(got, payload, "every byte must survive, 0x00 / 0x5c / 0x27 included");
}

/// PARAMS family: the same payload bound as `Value::Bytes` — the shape the
/// extended protocol produces from a node-pg `Buffer` parameter — must store
/// verbatim, come back verbatim from `RETURNING`, and read back verbatim.
#[test]
fn large_bytea_round_trips_through_the_params_family() {
    let db = mem_db();
    let payload = blob();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, b BYTEA)").unwrap();

    let (affected, returned) = db
        .execute_params_returning(
            "INSERT INTO t (id, b) VALUES ($1, $2) RETURNING b",
            &[Value::Int4(1), Value::Bytes(payload.clone())],
        )
        .unwrap();
    assert_eq!(affected, 1);
    assert_eq!(returned.len(), 1, "RETURNING must produce exactly one tuple");
    assert_eq!(
        bytes_of(returned[0].values.first().unwrap()),
        payload,
        "RETURNING must carry the STORED bytes, not a re-encoding of them"
    );

    let rows = db
        .query_params("SELECT b FROM t WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(bytes_of(rows[0].values.first().unwrap()), payload);
}

/// UPDATE … RETURNING on the params family — the second half of #25's item (2).
#[test]
fn large_bytea_survives_update_returning_on_the_params_family() {
    let db = mem_db();
    let payload = blob();
    let mut swapped = payload.clone();
    swapped.reverse();

    db.execute("CREATE TABLE t (id INT PRIMARY KEY, b BYTEA)").unwrap();
    db.execute_params_returning(
        "INSERT INTO t (id, b) VALUES ($1, $2) RETURNING id",
        &[Value::Int4(1), Value::Bytes(payload)],
    )
    .unwrap();

    let (affected, returned) = db
        .execute_params_returning(
            "UPDATE t SET b = $1 WHERE id = $2 RETURNING b",
            &[Value::Bytes(swapped.clone()), Value::Int4(1)],
        )
        .unwrap();
    assert_eq!(affected, 1);
    assert_eq!(
        returned.len(),
        1,
        "UPDATE … RETURNING must produce exactly one tuple — indexing [0] below \
         would otherwise panic with a message that hides the real failure"
    );
    assert_eq!(
        bytes_of(returned[0].values.first().unwrap()),
        swapped,
        "UPDATE … RETURNING must carry the POST-update bytes, in full"
    );
}

/// A NULL bytea followed by a real one — the engine-side control for the wire
/// test `gh25_simple_select_bytea_oid_survives_a_null_first_row`. The ENGINE
/// keeps the values straight; only the protocol layer's RowDescription typing
/// is fooled by the leading NULL, which is why the fix belongs there.
#[test]
fn null_then_value_keeps_both_rows_intact_on_both_families() {
    let payload = blob();
    for params_family in [false, true] {
        let db = mem_db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, b BYTEA)").unwrap();
        db.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
        if params_family {
            db.execute_params_returning(
                "INSERT INTO t (id, b) VALUES ($1, $2) RETURNING id",
                &[Value::Int4(2), Value::Bytes(payload.clone())],
            )
            .unwrap();
        } else {
            db.execute(&format!("INSERT INTO t VALUES (2, '{}')", hex_literal(&payload)))
                .unwrap();
        }

        let rows = db.query("SELECT b FROM t ORDER BY id", &[]).unwrap();
        assert_eq!(rows.len(), 2, "family={params_family}");
        assert!(
            matches!(rows[0].values.first(), Some(Value::Null)),
            "row 1 must still be NULL (family={params_family})"
        );
        assert_eq!(
            bytes_of(rows[1].values.first().unwrap()),
            payload,
            "row 2 must be intact behind the NULL (family={params_family})"
        );
    }
}
