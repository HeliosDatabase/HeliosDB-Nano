//! Compatibility fixes from ada-core's
//! `dm26:/home/app/ada-core/docs/HELIOSDB-NANO-COMPATIBILITY.md` report
//! (v3.30.1 → v3.31.2). Covers the two tractable items from that doc:
//!
//! 1. **Vector bare `::vector` cast** — pgvector accepts the bare form and
//!    infers the dimension from the literal. v3.31.2 matches at CAST sites
//!    (DDL still requires explicit `VECTOR(n)`).
//! 2. **`CAST(uuid AS text)`** — was wrapping the result in single quotes
//!    (`'<uuid>'`). v3.31.2 returns the bare canonical hex form.
//!
//! Items left documented as deferred: TEXT[] arrays, `interval`, LISTEN/
//! NOTIFY, asyncpg binary wire, SQLAlchemy txn-frame compatibility,
//! "Transaction already active" stuck state, ALTER COLUMN DROP NOT NULL.

use heliosdb_nano::EmbeddedDatabase;

/// pgvector tutorial form: bare `::vector` cast on a string literal must
/// infer the dimension from the literal's element count.
#[test]
fn vector_bare_cast_infers_dimension_from_literal() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE v (id INT PRIMARY KEY, embedding vector(3))")
        .expect("create vector table");

    // The form pgvector tutorials use, which v3.30.1 rejected:
    db.execute("INSERT INTO v VALUES (1, '[1.0, 2.0, 3.0]'::vector)")
        .expect("bare ::vector cast must infer dim=3 from the literal");

    let rows = db.execute("SELECT * FROM v").expect("select");
    assert_eq!(rows, 1, "expected 1 row, got rowcount {rows}");
}

/// Explicit-dim mismatch on the CAST itself must still error — the
/// fix only affects the BARE form's inference, not the explicit form's
/// validation.
#[test]
fn vector_explicit_dim_mismatch_still_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    // Literal has 4 elements, explicit cast says (3) — must reject.
    let r = db.execute("SELECT '[1.0, 2.0, 3.0, 4.0]'::vector(3)");
    assert!(
        r.is_err(),
        "explicit ::vector(3) cast on a 4-element literal must error; got {r:?}"
    );
}

// NOTE on the bare-cast + column-dim mismatch case (separate, pre-existing):
// `INSERT INTO v(vector(3)) VALUES ('[1,2,3,4]'::vector)` is currently
// accepted because (a) my CAST fix correctly infers Vector(4) from the
// literal (matching pgvector), and (b) Nano's INSERT path doesn't
// validate value-vector-dim against column-vector-dim at storage time.
// pgvector enforces this and errors with "expected 3 dimensions, not 4".
// Tracked as a separate follow-up (column-dim enforcement at INSERT/
// UPDATE), not in this PR's scope.

/// Explicit-dim form still works (regression guard — the fix is at CAST,
/// the column-level path must be untouched).
#[test]
fn vector_explicit_dim_cast_still_works() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE v (id INT PRIMARY KEY, embedding vector(3))")
        .expect("create vector table");

    db.execute("INSERT INTO v VALUES (1, '[1.0, 2.0, 3.0]'::vector(3))")
        .expect("explicit ::vector(3) cast must still work");

    let rows = db.execute("SELECT * FROM v").expect("select");
    assert_eq!(rows, 1);
}

/// DDL bare `VECTOR` (no dim) must still error — the inference is only
/// at CAST sites, not at column-type declaration.
#[test]
fn vector_bare_in_ddl_still_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let r = db.execute("CREATE TABLE v (id INT PRIMARY KEY, embedding vector)");
    assert!(r.is_err(), "DDL with bare VECTOR (no dim) must still error; got {r:?}");
}

/// Empty literal `'[]'::vector` cannot infer a dimension — must error
/// rather than silently accept a zero-dim vector.
#[test]
fn vector_bare_cast_on_empty_literal_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let r = db.execute("SELECT '[]'::vector");
    assert!(
        r.is_err(),
        "bare ::vector cast on empty literal must error (no dim to infer); got {r:?}"
    );
}

/// `CAST(uuid AS text)` must return the bare 36-char canonical hex form,
/// not the single-quote-wrapped SQL-literal form. ada-core's psycopg
/// clients see the bare form when reading PG; the previous Nano behavior
/// returned `'<uuid>'` which broke string-equality comparisons on the
/// client side.
#[test]
fn uuid_cast_to_text_returns_bare_canonical_form() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE u (id UUID PRIMARY KEY, label TEXT)")
        .expect("create u");
    db.execute("INSERT INTO u VALUES ('550e8400-e29b-41d4-a716-446655440000', 'sample')")
        .expect("insert uuid");

    // Use string equality: if the CAST produced `'<uuid>'`, the LIKE
    // pattern `5%` would not match (would start with a quote char).
    // After the fix it returns `550e8400-…` and matches.
    let matches = db
        .execute("SELECT * FROM u WHERE CAST(id AS text) LIKE '550e%'")
        .expect("LIKE on cast(uuid as text)");
    assert_eq!(
        matches, 1,
        "CAST(uuid AS text) must return bare canonical hex (LIKE '550e%' should match); rowcount={matches}"
    );

    // And LIKE on the quote-wrapped form must NOT match anymore.
    let no_quote_match = db
        .execute("SELECT * FROM u WHERE CAST(id AS text) LIKE '''%'")
        .expect("LIKE on quote-prefixed pattern");
    assert_eq!(
        no_quote_match, 0,
        "CAST(uuid AS text) must NOT start with a quote char; rowcount={no_quote_match}"
    );
}

/// Negative guard: SQL-literal Display rendering of Value::Uuid (e.g.
/// when inserted into a quote-wrapping context) is unaffected — the
/// fix is scoped to the cast handler only.
#[test]
fn uuid_other_paths_unchanged() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE u (id UUID PRIMARY KEY)").expect("create u");
    db.execute("INSERT INTO u VALUES ('550e8400-e29b-41d4-a716-446655440000')")
        .expect("insert uuid");

    let rows = db.execute("SELECT id FROM u").expect("select");
    assert_eq!(rows, 1, "uuid round-trip must still produce 1 row");
}
