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
// B27 — DEFAULT keyword in VALUES resolves the column's declared default
// ---------------------------------------------------------------------
#[test]
fn b27_default_in_values_resolves_column_default() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT, created_at TIMESTAMP DEFAULT now() NOT NULL)")?;
    // Drizzle's exact shape: DEFAULT for SERIAL id + for DEFAULT now() column
    db.execute("INSERT INTO t (id, name, created_at) VALUES (DEFAULT, 'alice', DEFAULT)")?;
    let rows = db.query("SELECT id, name, created_at FROM t", &[])?;
    assert_eq!(rows.len(), 1);
    // created_at must be a real Timestamp, not Null.
    assert!(!matches!(rows[0].values[2], Value::Null),
        "DEFAULT in VALUES should resolve to the column's default expression, got {:?}",
        rows[0].values[2]);
    Ok(())
}

#[test]
fn b27_default_for_serial_still_auto_fills() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, v TEXT)")?;
    // DEFAULT on a SERIAL column must still produce 1, 2, 3, ….
    db.execute("INSERT INTO t (id, v) VALUES (DEFAULT, 'a')")?;
    db.execute("INSERT INTO t (id, v) VALUES (DEFAULT, 'b')")?;
    let rows = db.query("SELECT id, v FROM t ORDER BY id", &[])?;
    assert_eq!(rows.len(), 2);
    let id1 = match rows[0].values[0] {
        Value::Int4(n) => n as i64,
        Value::Int8(n) => n,
        _ => panic!(),
    };
    let id2 = match rows[1].values[0] {
        Value::Int4(n) => n as i64,
        Value::Int8(n) => n,
        _ => panic!(),
    };
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    Ok(())
}

// ---------------------------------------------------------------------
// B28 — wire-only regression (tested via postgres-js smoke rather than
// core API, same as B19/B20).
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// B29 — canonical Drizzle SELECT shape returns the row
//
// Reporter claims the combination of:
//   (1) SELECT list = every column in schema order (unqualified)
//   (2) WHERE "t"."col" = $1     (table-qualified predicate)
//   (3) $1 = string bind parameter via extended-Q
// returns `[]`. This test pins the *post-substitution* SQL (exactly what
// `database.query()` sees after `substitute_parameters()` rewrites the
// placeholder) and asserts one row comes back. The wire-level side of
// B29 (extended-Q Parse/Bind/Execute) is covered by
// `server_mode_integration_test::test_b29_canonical_drizzle_shape`.
// ---------------------------------------------------------------------
#[test]
fn b29_canonical_drizzle_select_returns_row() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        r#"CREATE TABLE "users" (
             "id" SERIAL PRIMARY KEY,
             "email" TEXT NOT NULL UNIQUE,
             "password" TEXT NOT NULL,
             "created_at" TIMESTAMP DEFAULT now() NOT NULL
           )"#,
    )?;
    db.execute(
        r#"INSERT INTO "users" ("email","password")
           VALUES ('alice@example.com', '$2a$10$pw')"#,
    )?;
    // All three triggers: all cols in schema order (unqualified),
    // table-qualified WHERE, string literal in the predicate position
    // (mirrors substituted $1).
    let rows = db.query(
        r#"SELECT "id", "email", "password", "created_at"
             FROM "users"
            WHERE "users"."email" = 'alice@example.com'"#,
        &[],
    )?;
    assert_eq!(rows.len(), 1, "canonical Drizzle shape returned 0 rows");
    match &rows[0].values[1] {
        Value::String(s) => assert_eq!(s, "alice@example.com"),
        v => panic!("email column: expected String, got {:?}", v),
    }
    Ok(())
}

/// B29 root cause: a SELECT that returned `[]` before an
/// `INSERT ... RETURNING` must NOT continue to return `[]` from the
/// result cache after the insert. The extended-Q handler routes
/// `INSERT RETURNING` through `execute_returning` →
/// `execute_params_returning` → `execute_plan_with_params`, which
/// previously forgot to invalidate the cache — so a login probe
/// before register would poison every subsequent login with a stale
/// empty result (TimeTracker symptom: 401 in ~2 ms).
#[test]
fn b29_login_probe_then_register_then_login() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        r#"CREATE TABLE "users" (
             "id" SERIAL PRIMARY KEY,
             "email" TEXT NOT NULL UNIQUE,
             "password" TEXT NOT NULL,
             "created_at" TIMESTAMP DEFAULT now() NOT NULL
           )"#,
    )?;
    // 1) login probe against empty table — caches `[]` under the
    //    substituted SQL key.
    let probe = db.query(
        r#"SELECT "id", "email", "password", "created_at"
             FROM "users"
            WHERE "users"."email" = 'alice@example.com'"#,
        &[],
    )?;
    assert_eq!(probe.len(), 0);
    // 2) register via execute_returning — same code path the
    //    extended-Q handler takes for INSERT...RETURNING.
    let (count, returned) = db.execute_returning(
        r#"INSERT INTO "users" ("email","password")
             VALUES ('alice@example.com', 'pw')
           RETURNING "id", "email", "password", "created_at""#,
    )?;
    assert_eq!(count, 1);
    assert_eq!(returned.len(), 1);
    // 3) login with the exact same SQL — must NOT return the stale [].
    let login = db.query(
        r#"SELECT "id", "email", "password", "created_at"
             FROM "users"
            WHERE "users"."email" = 'alice@example.com'"#,
        &[],
    )?;
    assert_eq!(login.len(), 1, "B29: stale result_cache after INSERT RETURNING");
    Ok(())
}

#[test]
fn b29_qualified_predicate_matches_scan_row() -> Result<()> {
    // Minimal reproduction of the predicate-matching invariant:
    // a scan yields rows whose `source_table_name == Some("t")`; the
    // filter predicate references `Column { table: Some("t"), name }`.
    // `Schema::get_qualified_column_index` must resolve it.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE t (id INT, name TEXT)")?;
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")?;
    let rows = db.query(
        "SELECT id, name FROM t WHERE t.name = 'b'",
        &[],
    )?;
    assert_eq!(rows.len(), 1);
    match &rows[0].values[0] {
        Value::Int4(1 | 2 | 3) => {}
        v => panic!("unexpected id value: {:?}", v),
    }
    Ok(())
}


// ---------------------------------------------------------------------
// B31 — UPDATE / DELETE with table-qualified WHERE column
//
// Reporter: `UPDATE "time_entries" SET "notes"=$1 WHERE
// "time_entries"."id"=$2 RETURNING *` fails with
// `Column 'time_entries.id' not found in schema`. Unqualified form
// works. SELECT with the same qualified predicate works (B29 retest
// confirmed). Asymmetry: Update/Delete built their evaluator from the
// bare catalog schema without stamping `source_table_name` on each
// column, so `Schema::get_qualified_column_index` couldn't match.
// ---------------------------------------------------------------------
#[test]
fn b31_update_with_qualified_where_column() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "time_entries" ("id" SERIAL PRIMARY KEY, "notes" TEXT)"#)?;
    db.execute(r#"INSERT INTO "time_entries" ("notes") VALUES ('old')"#)?;
    let (n, rows) = db.execute_returning(
        r#"UPDATE "time_entries" SET "notes"='new' WHERE "time_entries"."id"=1 RETURNING *"#,
    )?;
    assert_eq!(n, 1);
    assert_eq!(rows.len(), 1);
    Ok(())
}

#[test]
fn b31_delete_with_qualified_where_column() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT, "v" TEXT)"#)?;
    db.execute(r#"INSERT INTO "t" ("id","v") VALUES (1,'a'), (2,'b')"#)?;
    let n = db.execute(r#"DELETE FROM "t" WHERE "t"."id"=1"#)?;
    assert_eq!(n, 1);
    let left = db.query(r#"SELECT "id" FROM "t""#, &[])?;
    assert_eq!(left.len(), 1);
    Ok(())
}

// ---------------------------------------------------------------------
// B32 — Timestamp/Date ↔ ISO-string implicit coercion
//
// Reporter: Drizzle's `gte(column, date)` helpers bind JavaScript
// Date instances as ISO 8601 strings (`"2026-04-23T00:00:00.000Z"`)
// against OID 1114 / OID 1082 columns. Stock Postgres implicitly
// casts the literal to TIMESTAMP / DATE; HeliosDB refused with
// `Cannot compare Timestamp(…) and String(…)`, blocking every
// analytics endpoint. Fix: coerce in the comparator using the same
// parser as the TIMESTAMP cast path.
// ---------------------------------------------------------------------
#[test]
fn b32_timestamp_vs_iso_string_comparison() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT, "ts" TIMESTAMP)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (1, '2026-04-22 15:02:34')"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (2, '2026-04-24 10:00:00')"#)?;
    let gte = db.query(
        r#"SELECT "id" FROM "t" WHERE "ts" >= '2026-04-23T00:00:00.000Z'"#,
        &[],
    )?;
    assert_eq!(gte.len(), 1);
    let lt = db.query(
        r#"SELECT "id" FROM "t" WHERE "ts" < '2026-04-23T00:00:00.000Z'"#,
        &[],
    )?;
    assert_eq!(lt.len(), 1);
    Ok(())
}

#[test]
fn b32_date_vs_iso_string_comparison() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT, "d" DATE)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (1, '2026-04-22')"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (2, '2026-04-24')"#)?;
    let rows = db.query(r#"SELECT "id" FROM "t" WHERE "d" >= '2026-04-23'"#, &[])?;
    assert_eq!(rows.len(), 1);
    Ok(())
}

// ---------------------------------------------------------------------
// B33 — parameterized LIMIT / OFFSET
//
// Reporter: `LIMIT $1 OFFSET $2` fails with
// `LIMIT/OFFSET must be a number`, blocking every Drizzle `.limit(N)`
// analytics endpoint. Wire trigger: postgres-js binds the literal as
// TEXT (OID 25 / unknown OID 0), so `substitute_parameters` renders it
// as `LIMIT '3'`, which the planner's old `expr_to_usize` rejected.
// In-process: the planner mapped the placeholder to `usize::MAX` but
// the Limit plan held a fixed usize, so the bound integer never made
// it to the operator.
//
// Fix: (1) planner accepts `SingleQuotedString` parseable as usize;
// (2) `LogicalPlan::Limit` gained `limit_param` / `offset_param`
// fields; the executor resolves them from bound parameters at
// execution time.
// ---------------------------------------------------------------------
#[test]
fn b33_parameterized_limit() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (1),(2),(3),(4),(5)"#)?;
    let rows = db.query_params(
        r#"SELECT "id" FROM "t" ORDER BY "id" LIMIT $1"#,
        &[Value::Int4(3)],
    )?;
    assert_eq!(rows.len(), 3);
    Ok(())
}

#[test]
fn b33_parameterized_limit_offset() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (1),(2),(3),(4),(5)"#)?;
    let rows = db.query_params(
        r#"SELECT "id" FROM "t" ORDER BY "id" LIMIT $1 OFFSET $2"#,
        &[Value::Int4(2), Value::Int4(2)],
    )?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn b33_quoted_string_limit_wire_substitution() -> Result<()> {
    // Simulates the wire path: `substitute_parameters` renders a
    // TEXT-bound parameter with surrounding single quotes, so the
    // planner must accept `LIMIT '3'`.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (1),(2),(3),(4),(5)"#)?;
    let rows = db.query(r#"SELECT "id" FROM "t" ORDER BY "id" LIMIT '3'"#, &[])?;
    assert_eq!(rows.len(), 3);
    Ok(())
}

// ---------------------------------------------------------------------
// B34 — UPDATE SET … = $1 silently stored NULL for TIMESTAMP columns
//
// Reporter: `UPDATE t SET ts_col = $1` via extended-Q with an ISO 8601
// string parameter returned 200 / row count 1, but the column ended
// up NULL. `sql.unsafe(same-sql + params)` worked. INSERT worked. Root
// cause: INSERT's value path auto-casts the evaluated value to the
// target column type; UPDATE's SET path didn't — the String was
// pushed straight into a Timestamp slot and serialized away.
//
// Fix: mirror INSERT's auto-cast in all three UPDATE SET paths
// (`execute_plan_with_params`, `execute_in_transaction_inner`, and the
// non-params RLS path).
// ---------------------------------------------------------------------
#[test]
fn b34_update_set_param_timestamp() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT PRIMARY KEY, "ts" TIMESTAMP, "updated_at" TIMESTAMP)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (1, NULL, NULL)"#)?;
    let (count, _rows) = db.execute_params_returning(
        r#"UPDATE "t" SET "ts" = $1, "updated_at" = $2 WHERE "id" = $3"#,
        &[
            Value::String("2026-04-23T10:00:00.000Z".to_string()),
            Value::String("2026-04-23T10:00:00.000Z".to_string()),
            Value::Int4(1),
        ],
    )?;
    assert_eq!(count, 1);
    let rows = db.query(r#"SELECT "ts", "updated_at" FROM "t""#, &[])?;
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(rows[0].values[0], Value::Timestamp(_)),
        "ts after UPDATE SET $1 should be Timestamp, got {:?}",
        rows[0].values[0],
    );
    assert!(
        matches!(rows[0].values[1], Value::Timestamp(_)),
        "updated_at after UPDATE SET $2 should be Timestamp, got {:?}",
        rows[0].values[1],
    );
    Ok(())
}

#[test]
fn b34_update_set_literal_iso_string() -> Result<()> {
    // Wire path after substitute_parameters renders the TEXT-typed
    // bind as a single-quoted literal in the SET expression.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("id" INT PRIMARY KEY, "ts" TIMESTAMP)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES (1, NULL)"#)?;
    db.execute(r#"UPDATE "t" SET "ts" = '2026-04-23T10:00:00.000Z' WHERE "id" = 1"#)?;
    let rows = db.query(r#"SELECT "ts" FROM "t""#, &[])?;
    assert!(
        matches!(rows[0].values[0], Value::Timestamp(_)),
        "expected Timestamp, got {:?}", rows[0].values[0],
    );
    Ok(())
}

// ---------------------------------------------------------------------
// B35 — GROUP BY / projection matching across qualifier styles + DATE/TIME
// comparison.
//
// Reporter: Drizzle's embedded-SQL idiom emits queries that mix
// unqualified column refs in SELECT / CASE bodies with table-qualified
// refs in GROUP BY / WHERE, e.g.:
//
//   select date("check_in"), sum(...)
//   from "time_entries"
//   where "time_entries"."workspace_id" = $1
//   group by date("time_entries"."check_in")
//
// Stock PG treats `"check_in"` and `"time_entries"."check_in"` as the
// same column when unambiguous; Nano matched SELECT items against
// GROUP BY with `PartialEq`, so the two `Column` variants didn't
// match and the projection emitted an un-rewritten
// `date(col "check_in")` that later tried to resolve against the
// aggregate's output schema → `Column 'check_in' not found in schema`.
//
// Second root cause found while reproducing: `compare_values` in the
// aggregate's GroupKey sort had no Date / Time / Interval / Numeric
// arms, so any two values of those types compared equal. GROUP BY on
// a DATE column always collapsed to a single group. ORDER BY on a
// DATE column was similarly broken.
//
// Fix: (1) qualifier-insensitive structural equivalence
// (`Planner::exprs_equivalent`) used in the projection-rewrite step;
// (2) add Date / Time / Interval / Numeric arms to `compare_values`.
// ---------------------------------------------------------------------
#[test]
fn b35_mixed_qualifier_group_by() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        r#"CREATE TABLE "time_entries" ("id" INT, "check_in" TIMESTAMP, "workspace_id" INT)"#,
    )?;
    db.execute(
        r#"INSERT INTO "time_entries" VALUES
            (1, '2026-04-22 10:00:00', 1),
            (2, '2026-04-22 15:00:00', 1),
            (3, '2026-04-23 09:00:00', 1)"#,
    )?;
    let rows = db.query(
        r#"select date("check_in"), count(*) from "time_entries"
           group by date("time_entries"."check_in")"#,
        &[],
    )?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn b35_both_qualified_group_by() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "time_entries" ("id" INT, "check_in" TIMESTAMP)"#)?;
    db.execute(
        r#"INSERT INTO "time_entries" VALUES
            (1, '2026-04-22 10:00:00'), (2, '2026-04-23 09:00:00')"#,
    )?;
    let rows = db.query(
        r#"select date("time_entries"."check_in"), count(*) from "time_entries"
           group by date("time_entries"."check_in")"#,
        &[],
    )?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn b35_both_unqualified_group_by() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "time_entries" ("id" INT, "check_in" TIMESTAMP)"#)?;
    db.execute(
        r#"INSERT INTO "time_entries" VALUES
            (1, '2026-04-22 10:00:00'), (2, '2026-04-23 09:00:00')"#,
    )?;
    let rows = db.query(
        r#"select date("check_in"), count(*) from "time_entries"
           group by date("check_in")"#,
        &[],
    )?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn b35_reporter_full_shape() -> Result<()> {
    // Reporter's exact Drizzle-emitted query, verbatim.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        r#"CREATE TABLE "time_entries" (
             "id" INT,
             "check_in" TIMESTAMP,
             "check_out" TIMESTAMP,
             "is_break" BOOLEAN,
             "workspace_id" INT
           )"#,
    )?;
    db.execute(
        r#"INSERT INTO "time_entries" VALUES
            (1, '2026-04-22 09:00:00', '2026-04-22 11:00:00', false, 1),
            (2, '2026-04-22 13:00:00', '2026-04-22 14:30:00', true,  1),
            (3, '2026-04-23 08:30:00', '2026-04-23 12:00:00', false, 1)"#,
    )?;
    let rows = db.query_params(
        r#"select date("check_in"),
                 sum(case when "check_out" is not null and "is_break" = false
                          then extract(epoch from ("check_out" - "check_in"))/60 else 0 end)
           from "time_entries"
           where ("time_entries"."workspace_id" = $1 and "time_entries"."check_in" >= $2)
           group by date("time_entries"."check_in")"#,
        &[Value::Int4(1), Value::String("2026-04-01 00:00:00".to_string())],
    )?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn b35_date_column_group_by_correctness() -> Result<()> {
    // Root-cause guard: grouping by a DATE column used to collapse to
    // a single group (compare_values lacked a Date/Date arm).
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "t" ("d" DATE)"#)?;
    db.execute(r#"INSERT INTO "t" VALUES ('2026-04-22'), ('2026-04-23'), ('2026-04-22')"#)?;
    let rows = db.query(r#"SELECT "d", count(*) FROM "t" GROUP BY "d""#, &[])?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

// ---------------------------------------------------------------------
// B36 — FK references with quoted identifiers + fast-path bypass
//
// Reporter: `INSERT INTO "workspaces" (name, owner_id) VALUES (…)`
// over the extended protocol failed with
// `ERROR: Table '"users"' does not exist`, while the unquoted form
// silently succeeded even when no parent row matched. Two bugs
// acting together:
//
// Root cause #1 — planner stored `ObjectName::to_string()` for the
// referenced table, which preserves the original quote characters.
// `REFERENCES "users"(id)` (Drizzle's default) produced an FK whose
// `references_table` was literally `"users"` (with the quotes), and
// the FK-check catalog lookup couldn't find any table by that name.
// Fixed by normalising the table name (and column names) at
// constraint-construction time.
//
// Root cause #2 — `try_fast_insert` skipped FK validation entirely
// and extracted the table name from the SQL text verbatim (so
// quoted identifiers fell out of the fast path). Fixed by
// (a) stripping surrounding quotes from the extracted table name,
// and (b) bailing to the normal path for any table with FK
// constraints so the validated Insert arm handles it.
// ---------------------------------------------------------------------
#[test]
fn b36_fk_insert_with_quoted_references() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "users" ("id" SERIAL PRIMARY KEY, "email" TEXT)"#)?;
    db.execute(
        r#"CREATE TABLE "workspaces" (
             "id" SERIAL PRIMARY KEY,
             "name" TEXT,
             "owner_id" INTEGER REFERENCES "users"("id")
           )"#,
    )?;
    let (_, rows) = db.execute_returning(
        r#"INSERT INTO "users" ("email") VALUES ('a') RETURNING "id""#,
    )?;
    let parent_id = match rows[0].values[0] {
        Value::Int4(n) => n as i64,
        Value::Int8(n) => n,
        _ => panic!("unexpected id type"),
    };
    db.execute_params_returning(
        r#"INSERT INTO "workspaces" ("name", "owner_id") VALUES ($1, $2)"#,
        &[Value::String("w".into()), Value::Int4(parent_id as i32)],
    )?;
    Ok(())
}

#[test]
fn b36_fk_violation_fires_on_unquoted_insert() -> Result<()> {
    // Fast-path regression guard: unquoted INSERT used to bypass FK
    // validation entirely, silently writing orphan rows.
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE users (id SERIAL PRIMARY KEY, email TEXT)"#)?;
    db.execute(
        r#"CREATE TABLE workspaces (
             id SERIAL PRIMARY KEY,
             name TEXT,
             owner_id INTEGER REFERENCES users(id)
           )"#,
    )?;
    let err = db.execute(
        r#"INSERT INTO workspaces (name, owner_id) VALUES ('w', 999)"#,
    );
    assert!(err.is_err(), "FK violation expected, got Ok");
    Ok(())
}

#[test]
fn b36_fk_violation_fires_on_quoted_insert() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "users" ("id" SERIAL PRIMARY KEY, "email" TEXT)"#)?;
    db.execute(
        r#"CREATE TABLE "workspaces" (
             "id" SERIAL PRIMARY KEY,
             "name" TEXT,
             "owner_id" INTEGER REFERENCES "users"("id")
           )"#,
    )?;
    let err = db.execute(
        r#"INSERT INTO "workspaces" ("name", "owner_id") VALUES ('w', 999)"#,
    );
    assert!(err.is_err(), "FK violation expected, got Ok");
    Ok(())
}

#[test]
fn b36_fk_succeeds_both_shapes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(r#"CREATE TABLE "users" ("id" SERIAL PRIMARY KEY, "email" TEXT)"#)?;
    db.execute(
        r#"CREATE TABLE "workspaces" (
             "id" SERIAL PRIMARY KEY,
             "name" TEXT,
             "owner_id" INTEGER REFERENCES "users"("id")
           )"#,
    )?;
    db.execute(r#"INSERT INTO "users" ("email") VALUES ('a')"#)?;
    db.execute(r#"INSERT INTO workspaces (name, owner_id) VALUES ('u', 1)"#)?;
    db.execute(r#"INSERT INTO "workspaces" ("name", "owner_id") VALUES ('q', 1)"#)?;
    let count = db.query(r#"SELECT count(*) FROM "workspaces""#, &[])?;
    match count[0].values[0] {
        Value::Int8(2) => Ok(()),
        ref v => panic!("expected 2 rows, got {:?}", v),
    }
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
