//! GH#15 — `UPDATE … WHERE pk = '<literal>'` must affect exactly the rows a
//! `SELECT` with the identical predicate returns.
//!
//! ## The defect, end to end
//!
//! A user reported that `UPDATE docs SET meta = '<two-key JSON>' WHERE id =
//! '<uuid>'` returned `UPDATE 0` and changed nothing, while `SELECT … WHERE id
//! = '<uuid>'` returned the row. Silent write loss, no error. The chain:
//!
//! 1. psql's simple protocol lands in the text DML family
//!    (`EmbeddedDatabase::execute` → `execute_in_transaction_inner`).
//! 2. `try_fast_update` bails on `set_clause.contains(',')` — and the reporter's
//!    SET value is a JSON object, so it contains a comma. That fast path is the
//!    only one that types a quoted UUID literal correctly, so **the comma is the
//!    hidden trigger**: the identical UPDATE with a comma-free SET value worked.
//! 3. Having bailed, the statement reached the planner's UPDATE arm, which takes
//!    a PK point lookup. `try_extract_pk_value` returned the parser's literal
//!    verbatim — a `Value::String` for anything quoted, with no coercion to the
//!    PK column's type.
//! 4. `ArtIndexManager::encode_value_into` encodes `Value::String` as its 36
//!    UTF-8 bytes but `Value::Uuid` as 16 raw bytes, so the probe key could
//!    never equal the stored key.
//! 5. The miss became `None => vec![]` — an EMPTY row set with no fallback to a
//!    scan. The predicate was never evaluated. `UPDATE 0`, no error.
//!
//! The planner's read path was never broken for the reported UUID case: it
//! already coerced the probe (then `coerce_index_lookup_value` in
//! `src/sql/executor/scan.rs`, since collapsed into the shared rule — see the
//! item #99 section at the bottom of this file) and, crucially, falls back to a
//! scan when it cannot. The read FAST path was, though — `try_fast_select` turns an index
//! miss straight into `Ok(vec![])` with no scan, and its own literal typing
//! (`fast_string_literal_value`) accepted only RFC-3339 timestamps. So
//! `SELECT * FROM t WHERE ts_pk = '2024-01-15 10:30:00'` returned zero rows.
//! Same defect, same statement family, opposite direction — which is why the
//! TIMESTAMP test below asserts the SELECT precondition before touching UPDATE.
//!
//! ## What is pinned here
//!
//! * The exact live repro (UUID PK, SET value **with** a comma) and its
//!   comma-free control, on UPDATE and on DELETE — including
//!   `DELETE … RETURNING`, which reaches the same point lookup.
//! * The rest of the class: every PK type whose ART encoding differs from the
//!   literal's `Value::String` form. DATE and TIMESTAMP are derived from the
//!   encoder, not guessed — `Value::Timestamp` encodes as RFC 3339
//!   (`2024-01-15T10:30:00+00:00`), which is not the bytes of the
//!   `'2024-01-15 10:30:00'` a client writes.
//! * The no-fallback shape itself: an **uncoercible** literal must not use the
//!   point lookup at all, while a successfully-coerced probe that misses really
//!   does mean absent (so `UPDATE … WHERE id = <nonexistent>` stays a point
//!   lookup and does NOT regress into a full scan).
//! * Both executor families. The params family
//!   (`execute_params` → `execute_plan_with_params_inner`) has no point lookup
//!   in its UPDATE/DELETE arms and already scanned, so it always behaved; it is
//!   pinned so it cannot silently acquire the same defect.
//!
//! Every test asserts BOTH halves — the reported affected-row count AND the row
//! contents afterwards. A fix that reports 1 while writing nothing, or that
//! writes while reporting 0, fails here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

/// A SET value containing a comma — the hidden trigger. Shaped like the
/// reporter's two-key JSON object.
const COMMA_SET_VALUE: &str = r#"{"status": "active", "tier": 2}"#;
/// The same payload with no comma anywhere, so `try_fast_update` accepts it.
const COMMA_FREE_SET_VALUE: &str = "plain-payload";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn texts(rows: &[Tuple], col: usize) -> Vec<String> {
    rows.iter()
        .map(|t| match t.values.get(col) {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected a string at column {col}, got {other:?} in {t:?}"),
        })
        .collect()
}

/// Every `meta` value in `docs`, read with no WHERE clause so no read fast path
/// or index probe can mask what is actually stored.
fn all_meta(db: &EmbeddedDatabase) -> Vec<String> {
    let rows = db.query("SELECT meta FROM docs", &[]).unwrap();
    texts(&rows, 0)
}

fn row_count(db: &EmbeddedDatabase, table: &str) -> usize {
    db.query(&format!("SELECT * FROM {table}"), &[]).unwrap().len()
}

/// How many rows a SELECT with `predicate` sees — the ground truth an UPDATE or
/// DELETE with the identical predicate must match.
fn select_matches(db: &EmbeddedDatabase, table: &str, predicate: &str) -> heliosdb_nano::Result<usize> {
    db.query(&format!("SELECT * FROM {table} WHERE {predicate}"), &[])
        .map(|rows| rows.len())
}

/// A literal with no valid encoding for the PK column must be INERT: the
/// statement either errors (PostgreSQL's answer — `invalid input syntax for
/// type uuid`) or reports 0 rows, and either way the table is untouched.
///
/// This is the assertion for the no-fallback shape. Before the fix such a
/// literal took the point lookup, missed, and was reported as "no such row";
/// the danger of fixing that by declining the point lookup is the opposite
/// failure — a scan whose predicate coerces differently and mutates a row the
/// key could never have addressed. `read_sql` projects one text column with no
/// WHERE clause, so the before/after comparison sees the real table.
fn assert_uncoercible_literal_is_inert(
    db: &EmbeddedDatabase,
    table: &str,
    set_col: &str,
    predicate: &str,
    read_sql: &str,
) {
    let before = texts(&db.query(read_sql, &[]).unwrap(), 0);

    // An error here is the strictly better answer (PostgreSQL rejects the
    // literal outright) and is accepted; silently claiming rows is not.
    if let Ok(affected) = db.execute(&format!(
        "UPDATE {table} SET {set_col} = 'inert, value' WHERE {predicate}"
    )) {
        assert_eq!(
            affected, 0,
            "GH#15: `WHERE {predicate}` on {table} claimed {affected} affected row(s) for a literal \
             that cannot be a key of that column"
        );
    }

    assert_eq!(
        texts(&db.query(read_sql, &[]).unwrap(), 0),
        before,
        "GH#15: `WHERE {predicate}` on {table} mutated a row it must not match"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `docs` with exactly one row, keyed by a random UUID. Returns the key as the
/// canonical hyphenated text a client would send.
fn uuid_docs() -> (EmbeddedDatabase, String) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE docs (id UUID PRIMARY KEY, meta TEXT NOT NULL)")
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    db.execute(&format!("INSERT INTO docs VALUES ('{id}', 'seed')"))
        .unwrap();
    assert_eq!(all_meta(&db), vec!["seed".to_string()], "fixture did not insert");
    (db, id)
}

// ---------------------------------------------------------------------------
// The exact live repro
// ---------------------------------------------------------------------------

/// The reported statement, verbatim in shape: UUID PK, SET value containing a
/// comma. Before the fix this reported `UPDATE 0` and left `meta` as 'seed'.
#[test]
fn gh15_uuid_pk_update_with_comma_in_set_value_writes_the_row() {
    let (db, id) = uuid_docs();
    let predicate = format!("id = '{id}'");

    // Ground truth first: the read path finds the row.
    assert_eq!(
        select_matches(&db, "docs", &predicate).unwrap(),
        1,
        "precondition: SELECT by UUID PK must see the row"
    );

    let affected = db
        .execute(&format!("UPDATE docs SET meta = '{COMMA_SET_VALUE}' WHERE {predicate}"))
        .unwrap();

    assert_eq!(
        affected, 1,
        "GH#15: UPDATE by UUID PK reported {affected} affected rows while SELECT sees 1"
    );
    assert_eq!(
        all_meta(&db),
        vec![COMMA_SET_VALUE.to_string()],
        "GH#15: UPDATE reported success but the stored row never changed"
    );
}

/// The comma-free control. This one always worked (it takes `try_fast_update`,
/// which types the UUID literal itself) and must keep working — it is the
/// difference that isolated the comma as the trigger.
#[test]
fn gh15_uuid_pk_update_without_comma_still_writes_the_row() {
    let (db, id) = uuid_docs();

    let affected = db
        .execute(&format!(
            "UPDATE docs SET meta = '{COMMA_FREE_SET_VALUE}' WHERE id = '{id}'"
        ))
        .unwrap();

    assert_eq!(affected, 1, "comma-free UPDATE by UUID PK regressed");
    assert_eq!(all_meta(&db), vec![COMMA_FREE_SET_VALUE.to_string()]);
}

/// A genuinely absent UUID must still report 0 and touch nothing — the coerced
/// probe misses, and a miss on a *successfully coerced* key really does mean
/// "no such row". (If this passed by falling back to a scan the fix would have
/// turned every point UPDATE into a table scan.)
#[test]
fn gh15_absent_uuid_pk_reports_zero_and_changes_nothing() {
    let (db, _id) = uuid_docs();
    let absent = uuid::Uuid::new_v4().to_string();

    let affected = db
        .execute(&format!(
            "UPDATE docs SET meta = '{COMMA_SET_VALUE}' WHERE id = '{absent}'"
        ))
        .unwrap();

    assert_eq!(affected, 0, "absent UUID PK must affect 0 rows");
    assert_eq!(all_meta(&db), vec!["seed".to_string()], "absent-PK UPDATE wrote anyway");
}

/// A literal that is not a UUID at all cannot be encoded as a probe key, so the
/// point lookup must be declined rather than reported as "no such row". The
/// observable contract is that it stays inert.
#[test]
fn gh15_uncoercible_uuid_literal_is_inert() {
    let (db, _id) = uuid_docs();
    assert_uncoercible_literal_is_inert(&db, "docs", "meta", "id = 'not-a-uuid'", "SELECT meta FROM docs");
}

// ---------------------------------------------------------------------------
// DELETE — the identical point lookup, the identical `None => vec![]`
// ---------------------------------------------------------------------------

#[test]
fn gh15_uuid_pk_delete_removes_the_row() {
    let (db, id) = uuid_docs();

    let affected = db.execute(&format!("DELETE FROM docs WHERE id = '{id}'")).unwrap();

    assert_eq!(affected, 1, "GH#15: DELETE by UUID PK reported {affected}, expected 1");
    assert_eq!(
        row_count(&db, "docs"),
        0,
        "DELETE reported success but the row survived"
    );
}

/// `DELETE … RETURNING` by PK reaches the same arm and must return the row it
/// deleted, not an empty set.
#[test]
fn gh15_uuid_pk_delete_returning_yields_the_row() {
    let (db, id) = uuid_docs();

    let returned = db
        .query(&format!("DELETE FROM docs WHERE id = '{id}' RETURNING meta"), &[])
        .unwrap();

    assert_eq!(
        texts(&returned, 0),
        vec!["seed".to_string()],
        "GH#15: DELETE … RETURNING by UUID PK returned {} row(s)",
        returned.len()
    );
    assert_eq!(
        row_count(&db, "docs"),
        0,
        "RETURNING produced a row that was not deleted"
    );
}

#[test]
fn gh15_absent_uuid_pk_delete_reports_zero() {
    let (db, _id) = uuid_docs();
    let absent = uuid::Uuid::new_v4().to_string();

    let affected = db.execute(&format!("DELETE FROM docs WHERE id = '{absent}'")).unwrap();

    assert_eq!(affected, 0);
    assert_eq!(row_count(&db, "docs"), 1, "absent-PK DELETE removed a row anyway");
}

// ---------------------------------------------------------------------------
// The rest of the class, derived from `encode_value_into`
// ---------------------------------------------------------------------------

/// DATE PK. `Value::Date` encodes as `d.to_string()`, which happens to equal the
/// canonical `'YYYY-MM-DD'` literal's bytes — so this one could pass by
/// accident. It is pinned anyway: the coercion must be what makes it work, and a
/// non-canonical spelling of the same date must not silently miss.
#[test]
fn gh15_date_pk_update_matches_select() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE days (day DATE PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO days VALUES ('2024-01-15', 'seed')").unwrap();

    assert_eq!(
        select_matches(&db, "days", "day = '2024-01-15'").unwrap(),
        1,
        "precondition: SELECT by DATE PK must see the row"
    );

    let affected = db
        .execute("UPDATE days SET note = 'has, comma' WHERE day = '2024-01-15'")
        .unwrap();
    assert_eq!(affected, 1, "GH#15: UPDATE by DATE PK reported {affected}, expected 1");

    let notes = db.query("SELECT note FROM days", &[]).unwrap();
    assert_eq!(texts(&notes, 0), vec!["has, comma".to_string()]);
}

/// TIMESTAMP PK — the clearest non-UUID member of the class. `Value::Timestamp`
/// encodes as RFC 3339 (`2024-01-15T10:30:00+00:00`); the literal a client
/// writes is `'2024-01-15 10:30:00'`, whose `Value::String` bytes differ. Both
/// halves are asserted, so "reported 1, wrote nothing" fails.
#[test]
fn gh15_timestamp_pk_update_matches_select() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE events (at TIMESTAMP PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO events VALUES ('2024-01-15 10:30:00', 'seed')")
        .unwrap();

    assert_eq!(
        select_matches(&db, "events", "at = '2024-01-15 10:30:00'").unwrap(),
        1,
        "precondition: SELECT by TIMESTAMP PK must see the row"
    );

    let affected = db
        .execute("UPDATE events SET note = 'has, comma' WHERE at = '2024-01-15 10:30:00'")
        .unwrap();
    assert_eq!(
        affected, 1,
        "GH#15: UPDATE by TIMESTAMP PK reported {affected}, expected 1"
    );

    let notes = db.query("SELECT note FROM events", &[]).unwrap();
    assert_eq!(texts(&notes, 0), vec!["has, comma".to_string()]);
}

/// BYTEA PK. `Value::Bytes` encodes as its raw bytes, which the text spelling
/// `'\x0102'` is not. This is deliberately NOT asserted as a silent-write-loss
/// case, because it is not one: `Evaluator::compare_values` has no Bytes↔String
/// arm (nor even a Bytes↔Bytes arm), so the READ path declines the same probe —
/// literally so since item #99, which pointed the read path at the very same
/// `coerce_literal_to_column_type`. BYTEA equality is a pre-existing, separate gap
/// on BOTH paths — see the follow-ups.
///
/// What GH#15 owes this type is only that the write path does not *invent* a
/// match the read path would not make: the text literal must stay inert.
#[test]
fn gh15_bytea_pk_text_literal_stays_inert() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE blobs (data BYTEA PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute_params(
        "INSERT INTO blobs VALUES ($1, $2)",
        &[Value::Bytes(vec![1, 2, 3]), Value::String("seed".to_string())],
    )
    .unwrap();
    assert_eq!(row_count(&db, "blobs"), 1, "fixture did not insert");

    assert_uncoercible_literal_is_inert(
        &db,
        "blobs",
        "note",
        "data = 'not-really-bytes'",
        "SELECT note FROM blobs",
    );
}

/// NUMERIC PK regression pin. An integer literal against a DECIMAL PK was an
/// earlier instance of this same class (the ART key is the decimal *string*
/// `"6"`, not the 4-byte int encoding) and its fix lived in the hand-rolled
/// `coerce_pk_value` arms that GH#15 replaced with the shared rule.
#[test]
fn numeric_pk_integer_literal_still_matches() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE prices (id DECIMAL PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO prices VALUES (6, 'seed')").unwrap();

    let affected = db
        .execute("UPDATE prices SET note = 'has, comma' WHERE id = 6")
        .unwrap();
    assert_eq!(
        affected, 1,
        "NUMERIC PK by integer literal regressed: reported {affected}"
    );

    let notes = db.query("SELECT note FROM prices", &[]).unwrap();
    assert_eq!(texts(&notes, 0), vec!["has, comma".to_string()]);
}

// ---------------------------------------------------------------------------
// Controls: the types that always worked must keep working
// ---------------------------------------------------------------------------

/// INT PK control, on both the comma (planner) and comma-free (fast) paths.
#[test]
fn int_pk_update_control_both_paths() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE nums (id INT PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO nums VALUES (1, 'seed')").unwrap();
    db.execute("INSERT INTO nums VALUES (2, 'seed')").unwrap();

    // Comma in the SET value → planner arm → PK point lookup.
    let affected = db.execute("UPDATE nums SET note = 'has, comma' WHERE id = 1").unwrap();
    assert_eq!(affected, 1, "INT PK planner path regressed");

    // No comma → `try_fast_update`.
    let affected = db.execute("UPDATE nums SET note = 'plain' WHERE id = 2").unwrap();
    assert_eq!(affected, 1, "INT PK fast path regressed");

    let mut notes = texts(&db.query("SELECT note FROM nums", &[]).unwrap(), 0);
    notes.sort();
    assert_eq!(notes, vec!["has, comma".to_string(), "plain".to_string()]);

    // Absent INT PK: 0 rows, nothing written.
    let affected = db.execute("UPDATE nums SET note = 'no, one' WHERE id = 99").unwrap();
    assert_eq!(affected, 0, "absent INT PK must affect 0 rows");
}

/// A BIGINT PK probed with a small integer literal: `Value::Int4(1)` is 4 bytes
/// but the stored `Value::Int8(1)` key is 8, so the widening coercion is
/// load-bearing. It moved into the shared rule; pin it.
#[test]
fn bigint_pk_narrow_literal_still_matches() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE big (id BIGINT PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO big VALUES (1, 'seed')").unwrap();

    let affected = db.execute("UPDATE big SET note = 'has, comma' WHERE id = 1").unwrap();
    assert_eq!(affected, 1, "BIGINT PK widening coercion regressed");
    assert_eq!(
        texts(&db.query("SELECT note FROM big", &[]).unwrap(), 0),
        vec!["has, comma".to_string()]
    );
}

/// An integer literal too wide for an INT4 PK has no valid key for that column.
/// It must never be truncated into a *different* row's key.
#[test]
fn out_of_range_int_literal_never_hits_another_row() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE nums (id INT PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO nums VALUES (1, 'seed')").unwrap();

    // 4294967297 == 2^32 + 1: truncating to i32 yields exactly 1, the row above.
    let affected = db
        .execute("UPDATE nums SET note = 'wrong, row' WHERE id = 4294967297")
        .unwrap();
    assert_eq!(affected, 0, "out-of-range INT literal matched a row it must not");
    assert_eq!(
        texts(&db.query("SELECT note FROM nums", &[]).unwrap(), 0),
        vec!["seed".to_string()],
        "out-of-range INT literal overwrote a different row"
    );
}

/// A composite PRIMARY KEY marks every member `primary_key = true`; probing the
/// grouped index with one value never matches. `try_extract_pk_value` declines
/// (BUG F) and the caller scans. GH#15 rewrote that function — pin the decline.
#[test]
fn composite_pk_prefix_predicate_still_scans() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE parts (a INT, b INT, note TEXT NOT NULL, PRIMARY KEY (a, b))")
        .unwrap();
    db.execute("INSERT INTO parts VALUES (1, 1, 'seed')").unwrap();
    db.execute("INSERT INTO parts VALUES (1, 2, 'seed')").unwrap();

    let affected = db.execute("UPDATE parts SET note = 'has, comma' WHERE a = 1").unwrap();
    assert_eq!(
        affected, 2,
        "composite-PK prefix UPDATE reported {affected}; both rows match"
    );
}

// ---------------------------------------------------------------------------
// The other executor family
// ---------------------------------------------------------------------------

/// `execute_params` / `execute_params_returning` (the PG extended protocol's
/// family) have no PK point lookup in their UPDATE/DELETE arms — they scan and
/// evaluate the predicate, so they never had this defect. Pinned so they cannot
/// acquire it, with the PK bound both as a typed `Value::Uuid` and as the
/// `Value::String` a client that sends UUIDs as text produces.
#[test]
fn params_family_uuid_pk_update_and_delete() {
    // Typed UUID parameter.
    let (db, id) = uuid_docs();
    let typed: uuid::Uuid = id.parse().unwrap();
    let affected = db
        .execute_params(
            "UPDATE docs SET meta = $1 WHERE id = $2",
            &[Value::String(COMMA_SET_VALUE.to_string()), Value::Uuid(typed)],
        )
        .unwrap();
    assert_eq!(
        affected, 1,
        "params family: UPDATE by typed UUID param reported {affected}"
    );
    assert_eq!(all_meta(&db), vec![COMMA_SET_VALUE.to_string()]);

    // Text-typed UUID parameter.
    let (db, id) = uuid_docs();
    let affected = db
        .execute_params(
            "UPDATE docs SET meta = $1 WHERE id = $2",
            &[Value::String(COMMA_SET_VALUE.to_string()), Value::String(id.clone())],
        )
        .unwrap();
    assert_eq!(
        affected, 1,
        "params family: UPDATE by string UUID param reported {affected}"
    );
    assert_eq!(all_meta(&db), vec![COMMA_SET_VALUE.to_string()]);

    // DELETE … RETURNING on the params family.
    let (db, id) = uuid_docs();
    let typed: uuid::Uuid = id.parse().unwrap();
    let (affected, returned) = db
        .execute_params_returning("DELETE FROM docs WHERE id = $1 RETURNING meta", &[Value::Uuid(typed)])
        .unwrap();
    assert_eq!(affected, 1, "params family: DELETE by UUID param reported {affected}");
    assert_eq!(texts(&returned, 0), vec!["seed".to_string()]);
    assert_eq!(
        row_count(&db, "docs"),
        0,
        "params family: RETURNING row was not deleted"
    );

    // Absent key on the params family still reports 0.
    let (db, _id) = uuid_docs();
    let absent = uuid::Uuid::new_v4();
    let affected = db
        .execute_params(
            "UPDATE docs SET meta = $1 WHERE id = $2",
            &[Value::String("nope".to_string()), Value::Uuid(absent)],
        )
        .unwrap();
    assert_eq!(affected, 0, "params family: absent UUID must affect 0 rows");
    assert_eq!(all_meta(&db), vec!["seed".to_string()]);
}

// ═══════════════════════════════════════════════════════════════════════════
// Item #99 — the READ path now shares the same one rule
//
// GH#15 (above) fixed the WRITE probe by routing it through
// `coerce_literal_to_column_type` (src/sql/executor/mod.rs). The READ probe in
// src/sql/executor/scan.rs kept a private near-clone, `coerce_index_lookup_value`
// plus four helpers — the same rule, written twice, with nothing keeping the two
// in step. A drift between the read probe and the write probe is EXACTLY the
// divergence that produced the write loss narrated at the top of this file, so
// the clone was deleted and scan.rs's four call sites now call the canonical
// function.
//
// The tests below are the no-regression evidence for that collapse. They assert
// the READ side: the same lookups, through both executor families, still return
// the same rows. The per-`DataType` answer itself is pinned as a unit test next
// to the function (`probe_coercion_tests` in src/sql/executor/mod.rs), so an
// edit to the rule fails there before it can reach here.
//
// The one behavioural delta the collapse introduces is on NUMERIC: the canonical
// rule coerces an INTEGER literal to `Numeric("6")` (the write path needs it —
// `get_row_by_pk` has no scan to fall back to), whereas the deleted clone
// declined. That is a NARROWING on the read side, because the ART key for a
// NUMERIC column is the raw decimal string, so `6` and a stored `6.00` are
// different keys for the same number. `try_index_lookup_for_scan` and
// `try_index_in_list_for_scan` therefore decline the fast path when a NUMERIC
// probe MISSES and fall back to the filtered scan, which compares numerically.
// ═══════════════════════════════════════════════════════════════════════════

/// Read-path no-regression matrix. For every PK type whose ART encoding differs
/// from a literal's `Value::String` form, a SELECT by the literal AND by a bound
/// parameter must still return exactly the seeded row — on BOTH executor
/// families (`query` → the text family, `query_params` → the extended-protocol
/// family every real driver uses).
///
/// This is the "AFTER column is identical to the BEFORE column" test: nothing
/// here is new behaviour, and that is the point.
#[test]
fn read_path_probe_matrix_survives_the_collapse() {
    let uuid_key = uuid::Uuid::new_v4();
    // (column DDL, literal as written in SQL, bound-parameter form)
    let cases: Vec<(&str, String, Value)> = vec![
        ("SMALLINT", "7".to_string(), Value::Int2(7)),
        ("INT", "7".to_string(), Value::Int4(7)),
        ("BIGINT", "7".to_string(), Value::Int8(7)),
        ("DOUBLE PRECISION", "2.5".to_string(), Value::Float8(2.5)),
        ("TEXT", "'abc'".to_string(), Value::String("abc".to_string())),
        ("VARCHAR(16)", "'abc'".to_string(), Value::String("abc".to_string())),
        ("UUID", format!("'{uuid_key}'"), Value::Uuid(uuid_key)),
        (
            "DATE",
            "'2024-01-15'".to_string(),
            Value::String("2024-01-15".to_string()),
        ),
        (
            "TIMESTAMP",
            "'2024-01-15 10:30:00'".to_string(),
            Value::String("2024-01-15 10:30:00".to_string()),
        ),
    ];

    for (ddl_type, literal, param) in cases {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(&format!(
            "CREATE TABLE probe (id {ddl_type} PRIMARY KEY, note TEXT NOT NULL)"
        ))
        .unwrap_or_else(|e| panic!("{ddl_type}: CREATE TABLE failed: {e}"));
        db.execute(&format!("INSERT INTO probe VALUES ({literal}, 'seed')"))
            .unwrap_or_else(|e| panic!("{ddl_type}: INSERT failed: {e}"));

        // Text family, literal predicate.
        let rows = db
            .query(&format!("SELECT note FROM probe WHERE id = {literal}"), &[])
            .unwrap_or_else(|e| panic!("{ddl_type}: SELECT by literal errored: {e}"));
        assert_eq!(
            texts(&rows, 0),
            vec!["seed".to_string()],
            "{ddl_type}: SELECT by the literal {literal} lost the row after the coercion collapse"
        );

        // Params family, bound parameter.
        let rows = db
            .query_params("SELECT note FROM probe WHERE id = $1", &[param.clone()])
            .unwrap_or_else(|e| panic!("{ddl_type}: SELECT by bound param errored: {e}"));
        assert_eq!(
            texts(&rows, 0),
            vec!["seed".to_string()],
            "{ddl_type}: SELECT by a bound parameter lost the row after the coercion collapse"
        );

        // A genuine non-match must still be 0 rows, not a manufactured row.
        let absent = match ddl_type {
            "TEXT" | "VARCHAR(16)" => "'zzz'".to_string(),
            "UUID" => format!("'{}'", uuid::Uuid::new_v4()),
            "DATE" => "'2029-12-31'".to_string(),
            "TIMESTAMP" => "'2029-12-31 00:00:00'".to_string(),
            _ => "31".to_string(),
        };
        let rows = db
            .query(&format!("SELECT note FROM probe WHERE id = {absent}"), &[])
            .unwrap_or_else(|e| panic!("{ddl_type}: SELECT for an absent key errored: {e}"));
        assert!(
            rows.is_empty(),
            "{ddl_type}: an absent key {absent} returned {} row(s)",
            rows.len()
        );
    }
}

/// The NUMERIC narrowing guard — the test a naive collapse fails.
///
/// The canonical rule turns the integer literal `6` into the probe key `"6"`,
/// but a row inserted as `6.00` is keyed `"6.00"`: same number, different bytes.
/// The probe misses, and if that miss were reported as "no such row" the SELECT
/// would silently lose a row the predicate matches. `try_index_lookup_for_scan`
/// declines the fast path instead and lets the filtered scan compare numerically.
#[test]
fn numeric_pk_integer_literal_read_survives_a_scaled_key() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE prices (id NUMERIC PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO prices VALUES (6.00, 'six')").unwrap();
    db.execute("INSERT INTO prices VALUES (7, 'seven')").unwrap();

    let rows = db.query("SELECT note FROM prices WHERE id = 6", &[]).unwrap();
    assert_eq!(
        texts(&rows, 0),
        vec!["six".to_string()],
        "NUMERIC PK: `= 6` must find the row stored as 6.00 — the ART key is the \
         decimal STRING, so the probe misses and must fall back to a scan"
    );

    // The unscaled row still resolves (probe hit; nothing changed for it).
    let rows = db.query("SELECT note FROM prices WHERE id = 7", &[]).unwrap();
    assert_eq!(texts(&rows, 0), vec!["seven".to_string()]);

    // Genuine absence is still 0 rows — the fallback must not manufacture any.
    let rows = db.query("SELECT note FROM prices WHERE id = 999", &[]).unwrap();
    assert!(
        rows.is_empty(),
        "NUMERIC PK: an absent key returned {} row(s)",
        rows.len()
    );

    // The GH#15 write-path win must not be given back. Asserted on the
    // canonically-keyed row (`7`): the write path is a `get_row_by_pk` with no
    // scan to fall back to, so a SCALED key (`6.00` probed as `6`) is a known,
    // separately filed limitation of the NUMERIC ART encoding — not something
    // this item claims to fix, and not something it may silently regress.
    let affected = db
        .execute("UPDATE prices SET note = 'has, comma' WHERE id = 7")
        .unwrap();
    assert_eq!(affected, 1, "NUMERIC PK UPDATE by integer literal regressed");
    assert_eq!(
        select_matches(&db, "prices", "id = 7").unwrap(),
        1,
        "the updated NUMERIC row must still be readable by the same predicate"
    );
    let affected = db.execute("DELETE FROM prices WHERE id = 7").unwrap();
    assert_eq!(affected, 1, "NUMERIC PK DELETE by integer literal regressed");
    assert_eq!(row_count(&db, "prices"), 1);
}

/// An INT PK miss must NOT pay the NUMERIC fallback: it stays a point lookup and
/// still reports nothing. (The fallback is keyed on `Value::Numeric`, which the
/// canonical rule produces only for a NUMERIC column, so no other type can reach
/// it. This pins the answer, which is what a user observes.)
#[test]
fn int_pk_miss_is_still_a_plain_zero_row_answer() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE nums (id INT PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO nums VALUES (1, 'seed')").unwrap();

    assert_eq!(select_matches(&db, "nums", "id = 99").unwrap(), 0);
    assert_eq!(select_matches(&db, "nums", "id = 1").unwrap(), 1);
}

/// IN-list pushdown (`indexed_in_list_lookup`) is the third scan.rs call site
/// that moved onto the canonical rule. Every element must resolve or the WHOLE
/// pushdown declines, because a partial probe set drops matching rows.
#[test]
fn in_list_pushdown_still_returns_the_right_rows() {
    // Coercible elements on an INT PK: probes, and both rows come back.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE nums (id INT PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO nums VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO nums VALUES (2, 'b')").unwrap();
    db.execute("INSERT INTO nums VALUES (3, 'c')").unwrap();
    let mut got = texts(&db.query("SELECT note FROM nums WHERE id IN (1, 2)", &[]).unwrap(), 0);
    got.sort();
    assert_eq!(got, vec!["a".to_string(), "b".to_string()]);

    // A UUID PK with quoted literals: the coercion is load-bearing here (36 text
    // bytes vs a 16-byte key) and both rows must come back.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE docs2 (id UUID PRIMARY KEY, note TEXT NOT NULL)")
        .unwrap();
    let u1 = uuid::Uuid::new_v4();
    let u2 = uuid::Uuid::new_v4();
    let u3 = uuid::Uuid::new_v4();
    db.execute(&format!("INSERT INTO docs2 VALUES ('{u1}', 'one')"))
        .unwrap();
    db.execute(&format!("INSERT INTO docs2 VALUES ('{u2}', 'two')"))
        .unwrap();
    db.execute(&format!("INSERT INTO docs2 VALUES ('{u3}', 'three')"))
        .unwrap();

    let mut got = texts(
        &db.query(&format!("SELECT note FROM docs2 WHERE id IN ('{u1}', '{u2}')"), &[])
            .unwrap(),
        0,
    );
    got.sort();
    assert_eq!(got, vec!["one".to_string(), "two".to_string()]);

    // One uncoercible element declines the whole pushdown. Whatever the scan
    // then does with `'not-a-uuid'`, it must never return a WRONG row set:
    // either the two real rows, or a loud error. Silently answering with a
    // different set is the failure this asserts against.
    match db.query(
        &format!("SELECT note FROM docs2 WHERE id IN ('{u1}', '{u2}', 'not-a-uuid')"),
        &[],
    ) {
        Ok(rows) => {
            let mut got = texts(&rows, 0);
            got.sort();
            assert_eq!(
                got,
                vec!["one".to_string(), "two".to_string()],
                "an uncoercible IN element changed which real rows matched"
            );
        }
        Err(_) => { /* rejecting the invalid UUID literal outright is also correct */ }
    }
}

/// Index RANGE scans are the fourth scan.rs call site. `range_scannable_type`
/// admits only the order-preserving encodings (never NUMERIC), so the range path
/// cannot reach the one arm that changed — pin that it returns the same rows.
#[test]
fn index_range_scan_is_unaffected_by_the_collapse() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE scores (id INT PRIMARY KEY, n INT NOT NULL, label TEXT NOT NULL)")
        .unwrap();
    db.execute("CREATE INDEX scores_n_idx ON scores (n)").unwrap();
    db.execute("CREATE INDEX scores_label_idx ON scores (label)").unwrap();
    for (id, n, label) in [(1, 1, "alpha"), (2, 5, "mike"), (3, 10, "zulu"), (4, 20, "delta")] {
        db.execute(&format!("INSERT INTO scores VALUES ({id}, {n}, '{label}')"))
            .unwrap();
    }

    let mut got = texts(
        &db.query("SELECT label FROM scores WHERE n BETWEEN 5 AND 10", &[])
            .unwrap(),
        0,
    );
    got.sort();
    assert_eq!(got, vec!["mike".to_string(), "zulu".to_string()]);

    let mut got = texts(&db.query("SELECT label FROM scores WHERE n > 5", &[]).unwrap(), 0);
    got.sort();
    assert_eq!(got, vec!["delta".to_string(), "zulu".to_string()]);

    let mut got = texts(
        &db.query("SELECT label FROM scores WHERE label >= 'm'", &[]).unwrap(),
        0,
    );
    got.sort();
    assert_eq!(got, vec!["mike".to_string(), "zulu".to_string()]);

    // Same through the extended-protocol family.
    let mut got = texts(
        &db.query_params(
            "SELECT label FROM scores WHERE n BETWEEN $1 AND $2",
            &[Value::Int4(5), Value::Int4(10)],
        )
        .unwrap(),
        0,
    );
    got.sort();
    assert_eq!(got, vec!["mike".to_string(), "zulu".to_string()]);
}
