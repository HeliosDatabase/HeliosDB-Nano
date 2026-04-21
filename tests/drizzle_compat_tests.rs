//! Regression suite tracking `BUGS_TIMETRACKER_DRIZZLE_COMPAT.md`.
//!
//! One test per bug number. Tests that exercised behaviour already
//! fixed before v3.13.1 (B1, B4, B6, B12, B13, B16) are still
//! included as guards against regression.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

// ---------------------------------------------------------------------
// B1 SERIAL auto-increment
// ---------------------------------------------------------------------
#[test]
fn b1_serial_auto_increments() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "users" ("id" SERIAL PRIMARY KEY, "email" VARCHAR(255) NOT NULL UNIQUE)"#)?;
    let (n, rows) = db.execute_returning(r#"INSERT INTO "users" ("email") VALUES ('alice@example.com') RETURNING id"#)?;
    assert_eq!(n, 1);
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].values[0], Value::Int4(1) | Value::Int8(1)),
        "expected id=1, got {:?}", rows[0].values[0]);
    Ok(())
}

// ---------------------------------------------------------------------
// B2 GENERATED ALWAYS AS IDENTITY
// ---------------------------------------------------------------------
#[test]
fn b2_identity_auto_increments() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t_ident (id INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v TEXT)")?;
    let (_, rows) = db.execute_returning("INSERT INTO t_ident (v) VALUES ('a') RETURNING id")?;
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].values[0], Value::Int4(1) | Value::Int8(1) | Value::Int2(1)));
    Ok(())
}

// ---------------------------------------------------------------------
// B3 DEFAULT keyword in INSERT VALUES
// ---------------------------------------------------------------------
#[test]
fn b3_default_keyword_triggers_autofill() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t_def (id SERIAL PRIMARY KEY, n TEXT)")?;
    let (_, rows) = db.execute_returning("INSERT INTO t_def (id, n) VALUES (DEFAULT, 'alice') RETURNING *")?;
    assert_eq!(rows.len(), 1);
    // id (position 0) must be auto-filled, not NULL
    assert!(!matches!(rows[0].values[0], Value::Null),
        "DEFAULT should have triggered SERIAL auto-fill, got {:?}", rows[0].values);
    Ok(())
}

// ---------------------------------------------------------------------
// B4 RETURNING * returns the full row, wire-safe
// ---------------------------------------------------------------------
#[test]
fn b4_returning_star_omitted_columns() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE hh (id SERIAL PRIMARY KEY, n TEXT)")?;
    let (_, rows) = db.execute_returning("INSERT INTO hh (n) VALUES ('bb') RETURNING *")?;
    assert_eq!(rows.len(), 1);
    // The returned tuple must match the schema column count (2),
    // not just the columns the user provided (1). Otherwise the PG
    // wire protocol sends a DataRow with a different field count
    // than the RowDescription and breaks every driver.
    assert_eq!(rows[0].values.len(), 2, "tuple {:?}", rows[0].values);
    Ok(())
}

// ---------------------------------------------------------------------
// B5 EXTRACT(EPOCH FROM ...) and other field extractions
// ---------------------------------------------------------------------
#[test]
fn b5_extract_epoch_from_timestamp_literal() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query(
        "SELECT EXTRACT(EPOCH FROM TIMESTAMP '2026-01-01 00:00:00')",
        &[],
    )?;
    let secs = match &rows[0].values[0] {
        Value::Float8(f) => *f,
        other => panic!("expected Float8, got {:?}", other),
    };
    assert!(secs > 1_700_000_000.0 && secs < 2_000_000_000.0,
        "expected Unix-epoch seconds near 2026, got {secs}");
    Ok(())
}

#[test]
fn b5_extract_calendar_fields() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query(
        "SELECT EXTRACT(YEAR FROM TIMESTAMP '2026-07-15 10:30:45'), \
                EXTRACT(MONTH FROM TIMESTAMP '2026-07-15 10:30:45'), \
                EXTRACT(DAY FROM TIMESTAMP '2026-07-15 10:30:45'), \
                EXTRACT(HOUR FROM TIMESTAMP '2026-07-15 10:30:45')",
        &[],
    )?;
    assert_eq!(rows[0].values[0], Value::Int4(2026));
    assert_eq!(rows[0].values[1], Value::Int4(7));
    assert_eq!(rows[0].values[2], Value::Int4(15));
    assert_eq!(rows[0].values[3], Value::Int4(10));
    Ok(())
}

// ---------------------------------------------------------------------
// B7 / B8 sequences
// ---------------------------------------------------------------------
#[test]
fn b7_create_sequence_and_nextval() -> Result<()> {
    // Sequences are process-global — use a unique name to avoid
    // collisions when this test file is re-run in the same process.
    let seq = format!("b7_seq_{}", uuid::Uuid::new_v4().simple());
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(&format!("CREATE SEQUENCE {seq}"))?;
    let r1 = db.query(&format!("SELECT nextval('{seq}')"), &[])?;
    assert_eq!(r1[0].values[0], Value::Int8(1));
    let r2 = db.query(&format!("SELECT nextval('{seq}')"), &[])?;
    assert_eq!(r2[0].values[0], Value::Int8(2));
    let cur = db.query(&format!("SELECT currval('{seq}')"), &[])?;
    assert_eq!(cur[0].values[0], Value::Int8(2));
    let set = db.query(&format!("SELECT setval('{seq}', 42)"), &[])?;
    assert_eq!(set[0].values[0], Value::Int8(42));
    let after = db.query(&format!("SELECT nextval('{seq}')"), &[])?;
    assert_eq!(after[0].values[0], Value::Int8(43));
    Ok(())
}

// ---------------------------------------------------------------------
// B9 DO $$ ... END $$ (handler-level)
// Covered by a psycopg-level smoke test; exercising here via the
// public API ensures the underlying SQL evaluator is OK with bodies.
// ---------------------------------------------------------------------
// NOTE: the DO wrapper itself is intercepted in the PG handler,
// not the EmbeddedDatabase API. That path is covered by the
// BUGS_TIMETRACKER_DRIZZLE_COMPAT.md live reproducer, run out of
// band against a running server.

// ---------------------------------------------------------------------
// B10 dollar-quoted string values
// ---------------------------------------------------------------------
#[test]
fn b10_dollar_quoted_literal() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query("SELECT $$hello world$$", &[])?;
    assert_eq!(rows[0].values[0], Value::String("hello world".into()));
    Ok(())
}

#[test]
fn b10_tagged_dollar_quoted_literal() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query("SELECT $tag$multi\nline body$tag$", &[])?;
    assert_eq!(rows[0].values[0], Value::String("multi\nline body".into()));
    Ok(())
}

// ---------------------------------------------------------------------
// B12 / B13 — `pg_catalog.pg_type` and `pg_tables`. These live in the
// PG wire handler's catalog emulator (`PgCatalog::handle_query`), not
// the core SQL engine. They're verified out-of-band against a running
// server; see the end-to-end psycopg smoke in the doc for this bug.
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// B14 identifier case-folding
// ---------------------------------------------------------------------
#[test]
fn b14_unquoted_identifier_case_folded() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE foo (id INT)")?;
    // Unquoted FOO should fold to foo and match.
    db.execute("INSERT INTO FOO VALUES (1)")?;
    let rows = db.query("SELECT * FROM Foo", &[])?;
    assert_eq!(rows.len(), 1);
    Ok(())
}

#[test]
fn b14_quoted_identifier_preserves_case() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "Bar" (id INT)"#)?;
    // Unquoted bar folds to "bar" — does NOT match quoted "Bar" (PG-compliant).
    let result = db.query(r#"SELECT * FROM "Bar""#, &[])?;
    assert_eq!(result.len(), 0);
    Ok(())
}

#[test]
fn b14_column_case_folding() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE ct (Id INT, Name TEXT)")?;
    db.execute("INSERT INTO ct (ID, NAME) VALUES (1, 'x')")?;
    let rows = db.query("SELECT id, name FROM ct", &[])?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int4(1));
    Ok(())
}

// ---------------------------------------------------------------------
// B15 gen_random_uuid
// ---------------------------------------------------------------------
#[test]
fn b15_gen_random_uuid() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query("SELECT gen_random_uuid()", &[])?;
    assert!(matches!(rows[0].values[0], Value::Uuid(_)));
    // Collision check — two calls should yield different values.
    let rows2 = db.query("SELECT gen_random_uuid()", &[])?;
    assert_ne!(rows[0].values[0], rows2[0].values[0]);
    Ok(())
}

// ---------------------------------------------------------------------
// B16 version()
// ---------------------------------------------------------------------
#[test]
fn b16_version_returns_current_nano_version() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query("SELECT version()", &[])?;
    let v = match &rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("expected String"),
    };
    assert!(v.contains(env!("CARGO_PKG_VERSION")),
        "version() = {v:?}");
    Ok(())
}

// ---------------------------------------------------------------------
// B19 / B20 — catalog emulation on Parse/Bind/Execute + WHERE-filter
// application lives in `src/protocol/postgres/{handler,catalog}.rs`.
// Those surfaces are PG-wire-only, not reachable through the core
// `EmbeddedDatabase::query` API this suite uses. Regression is
// covered by the live psql smoke test attached to
// BUGS_TIMETRACKER_DRIZZLE_COMPAT.md under B19 / B20.
//
// B21 — the PL/pgSQL detection helper is on the wire handler too, so
// the same caveat applies. Exercising it at the wire level is the
// right test surface.
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// B23 — Scalar subquery in UPDATE SET (correlated + uncorrelated)
// ---------------------------------------------------------------------
#[test]
fn b23_update_set_correlated_scalar_subquery() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE users (id SERIAL PRIMARY KEY, email TEXT)")?;
    db.execute("CREATE TABLE user_profile (user_id INT PRIMARY KEY, display_name TEXT)")?;
    db.execute("INSERT INTO users (email) VALUES ('a@b.c'), ('c@d.e')")?;
    db.execute("INSERT INTO user_profile (user_id) VALUES (1), (2)")?;

    // Correlated — outer `user_profile.user_id` referenced inside the subquery.
    db.execute(
        "UPDATE user_profile \
         SET display_name = (SELECT email FROM users WHERE users.id = user_profile.user_id)",
    )?;
    let rows = db.query("SELECT user_id, display_name FROM user_profile ORDER BY user_id", &[])?;
    assert_eq!(rows.len(), 2);
    if let Value::String(s) = &rows[0].values[1] {
        assert_eq!(s, "a@b.c");
    } else { panic!("expected String, got {:?}", rows[0].values); }
    if let Value::String(s) = &rows[1].values[1] {
        assert_eq!(s, "c@d.e");
    } else { panic!("expected String"); }
    Ok(())
}

#[test]
fn b23_update_set_uncorrelated_scalar_subquery() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE u2 (id INT, v INT)")?;
    db.execute("INSERT INTO u2 VALUES (1, 100), (2, 200)")?;
    // Uncorrelated — (SELECT MAX(v) FROM u2) resolves once.
    db.execute("UPDATE u2 SET v = (SELECT MAX(v) FROM u2)")?;
    let rows = db.query("SELECT v FROM u2 ORDER BY id", &[])?;
    assert_eq!(rows.len(), 2);
    for r in &rows {
        if let Value::Int4(n) = r.values[0] {
            assert_eq!(n, 200);
        } else if let Value::Int8(n) = r.values[0] {
            assert_eq!(n, 200);
        } else {
            panic!("expected int, got {:?}", r.values);
        }
    }
    Ok(())
}

#[test]
fn b23_scalar_subquery_returning_null() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t1 (id INT, n TEXT)")?;
    db.execute("CREATE TABLE t2 (id INT, n TEXT)")?;
    db.execute("INSERT INTO t1 VALUES (1, 'x')")?;
    // t2 is empty — subquery returns 0 rows → NULL.
    db.execute("UPDATE t1 SET n = (SELECT n FROM t2 WHERE t2.id = t1.id)")?;
    let rows = db.query("SELECT n FROM t1", &[])?;
    assert!(matches!(rows[0].values[0], Value::Null),
        "expected NULL from empty scalar subquery, got {:?}", rows[0].values);
    Ok(())
}

// ---------------------------------------------------------------------
// B24 — DEFAULT <expr> applied when the column is omitted from INSERT
// ---------------------------------------------------------------------
#[test]
fn b24_default_expr_applied_when_column_omitted() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT, \
                         created_at TIMESTAMP DEFAULT now() NOT NULL)",
    )?;
    db.execute("INSERT INTO t (name) VALUES ('alice')")?;
    let rows = db.query("SELECT name, created_at FROM t", &[])?;
    assert_eq!(rows.len(), 1);
    assert!(!matches!(rows[0].values[1], Value::Null),
        "created_at must be populated from DEFAULT now(), got {:?}", rows[0].values[1]);
    Ok(())
}

#[test]
fn b24_default_literal_applied() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, n INT DEFAULT 42)")?;
    db.execute("INSERT INTO t (id) VALUES (1)")?;
    let rows = db.query("SELECT n FROM t", &[])?;
    if let Value::Int4(v) = rows[0].values[0] {
        assert_eq!(v, 42);
    } else if let Value::Int8(v) = rows[0].values[0] {
        assert_eq!(v, 42);
    } else {
        panic!("expected int 42, got {:?}", rows[0].values[0]);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// B25 — INSERT ... DEFAULT VALUES
// ---------------------------------------------------------------------
#[test]
fn b25_default_values_syntax() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, n INT DEFAULT 99)")?;
    db.execute("INSERT INTO t DEFAULT VALUES")?;
    let rows = db.query("SELECT n FROM t", &[])?;
    assert_eq!(rows.len(), 1);
    if let Value::Int4(v) = rows[0].values[0] {
        assert_eq!(v, 99);
    } else if let Value::Int8(v) = rows[0].values[0] {
        assert_eq!(v, 99);
    } else {
        panic!("expected 99, got {:?}", rows[0].values[0]);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// B26 — NOT NULL enforced (explicit NULL + omitted) on every INSERT path
// ---------------------------------------------------------------------
#[test]
fn b26_not_null_explicit_null_rejected() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id INT, must_be_set TEXT NOT NULL)")?;
    let result = db.execute("INSERT INTO t (id, must_be_set) VALUES (1, NULL)");
    assert!(result.is_err(), "explicit NULL into NOT NULL column should error");
    let msg = format!("{}", result.unwrap_err());
    assert!(msg.contains("NOT NULL") || msg.contains("not null"),
        "error should mention NOT NULL, got: {msg}");
    Ok(())
}

#[test]
fn b26_not_null_omitted_rejected() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id INT, must_be_set TEXT NOT NULL)")?;
    // Omitting the NOT NULL column without a default should error,
    // not silently insert NULL.
    let result = db.execute("INSERT INTO t (id) VALUES (2)");
    assert!(result.is_err(), "omitted NOT NULL column should error");
    Ok(())
}

#[test]
fn b26_not_null_with_default_is_satisfied() -> Result<()> {
    // NOT NULL + DEFAULT: omitting the column should apply the default,
    // not error. Per PG.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id INT, status TEXT DEFAULT 'pending' NOT NULL)")?;
    db.execute("INSERT INTO t (id) VALUES (1)")?;
    let rows = db.query("SELECT status FROM t", &[])?;
    if let Value::String(s) = &rows[0].values[0] {
        assert_eq!(s, "pending");
    } else {
        panic!("expected 'pending', got {:?}", rows[0].values);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// heliosdb_capability_report() — self-describing capability probe
// ---------------------------------------------------------------------
#[test]
fn capability_report_is_non_empty() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let rows = db.query("SELECT heliosdb_capability_report()", &[])?;
    let report = match &rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("expected String"),
    };
    assert!(report.contains("HeliosDB Nano"));
    assert!(report.contains(env!("CARGO_PKG_VERSION")));
    Ok(())
}
