//! HDB-005 — `dump_sql` produced a file that could not be restored.
//!
//! The report, reproduced on v4.31.1 and d44d4eb:
//!
//! ```text
//! CREATE TABLE IF NOT EXISTS my table (      -- identifier never quoted
//!   id INT4 PRIMARY KEY,
//!   payload JSONB
//! );
//! INSERT INTO my table VALUES
//!   (1, 'Json("[\"x\"]")'),                  -- Rust debug rendering
//!   (2, 'Numeric("3.14159")'),
//!   (3, 'Uuid(2f4a…)'),
//!   (4, 'Array([String("p"), String("q")])');
//! ```
//!
//! `DumpManager::create_sql_dump` wrote every identifier bare and fell through
//! to `format!("'{:?}'", value)` for every `Value` variant it had not
//! enumerated — JSON, NUMERIC, UUID, BYTEA, DATE/TIME, arrays and vectors all
//! came out as Rust debug strings inside quotes. The REPL's `\dump` had a
//! SECOND copy of the same code with the same defects plus two of its own: it
//! printed the column TYPE with `{:?}` (`Varchar(Some(50))`) and emitted
//! `-- Row data would go here (n)` in place of every row, so the file did not
//! even contain the data. Neither writer emitted DEFAULTs, constraints or
//! indexes.
//!
//! The fix is one schema-aware serializer (`storage::dump::sql_text`) used by
//! both, plus `EmbeddedDatabase::execute_sql_script` as the matching restore
//! path. Each test below is a ROUND TRIP: populate a database, `dump_sql`,
//! replay the file into a fresh one, then compare the rows as typed `Value`s
//! and the `information_schema.columns` metadata between the two.
//!
//! Expected on the unfixed tree: every round-trip test fails — `execute` on
//! the very first `CREATE TABLE IF NOT EXISTS my table (` is a syntax error,
//! and where the identifier happens to need no quoting the values are restored
//! as the literal text `Json("[\"x\"]")`.
//!
//! ## Deliberately not covered
//!
//! * **INTERVAL columns.** `Evaluator::cast_value` has no `DataType::Interval`
//!   arm and the INSERT coercion gate casts every value whose variant is not on
//!   its short identity list, so INSERT into an INTERVAL column fails whatever
//!   literal is written — a pre-existing engine gap, asserted as such by
//!   `tests/value_rendering_tests.rs::interval_round_trips_as_an_expression`.
//!   The serializer's `INTERVAL '<n> microseconds'` rendering is covered by the
//!   unit tests in `src/storage/dump/sql_text.rs`.
//! * **Nested arrays.** The engine has no input syntax for them at all: the
//!   `ARRAY[…]` planner arm rejects a non-literal element, and
//!   `parse_pg_array_text_literal` splits `,` at one brace level only.
//! * **SERIAL / IDENTITY columns.** The dump writes the resolved `INT4`, so the
//!   restored column is not an identity column and its
//!   `information_schema.columns.column_default` differs (`nextval(…)` vs
//!   NULL). Recorded as a known gap, not asserted here.
//! * **Per-column STORAGE modes.** `STORAGE DICTIONARY` is not re-declared on
//!   restore (test 9 asserts only that the VALUE is exported resolved, which is
//!   what the report asked for).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::repl::MetaCommand;
use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};
use std::fs;
use tempfile::TempDir;

// ===========================================================================
// Round-trip harness
// ===========================================================================

struct RoundTrip {
    source: EmbeddedDatabase,
    restored: EmbeddedDatabase,
    script: String,
    /// Kept alive so the dump file outlives the dump.
    _dir: TempDir,
}

/// Build a database from `setup`, dump it to SQL, replay the file into a fresh
/// in-memory database, and hand back both plus the script.
fn round_trip(setup: &[&str]) -> RoundTrip {
    let source = EmbeddedDatabase::new_in_memory().expect("source database");
    for statement in setup {
        source
            .execute(statement)
            .unwrap_or_else(|e| panic!("setup failed: {statement}\n  {e}"));
    }

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("dump.sql");
    source.dump_sql(&path).expect("dump_sql");
    let script = fs::read_to_string(&path).expect("read dump");

    let restored = EmbeddedDatabase::new_in_memory().expect("restored database");
    restored
        .execute_sql_script(&script)
        .unwrap_or_else(|e| panic!("restore failed: {e}\n----- dump -----\n{script}"));

    RoundTrip {
        source,
        restored,
        script,
        _dir: dir,
    }
}

/// Dump `db` to SQL and hand back the script (the tempdir is dropped with it).
fn dump_script(db: &EmbeddedDatabase, label: &str) -> String {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("dump.sql");
    db.dump_sql(&path)
        .unwrap_or_else(|e| panic!("{label} dump_sql failed: {e}"));
    fs::read_to_string(&path).expect("read dump")
}

/// The STATEMENT lines of a script — everything that is not a `-- …` comment
/// or blank. The header carries a generation timestamp, so two dumps of the
/// same database are byte-identical only once it is dropped.
fn statement_lines(script: &str) -> Vec<&str> {
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .collect()
}

/// Bit-exact value equality.
///
/// Stricter than `PartialEq for Value`, which compares floats with IEEE
/// semantics: `NaN != NaN` (so a NaN round trip could never be asserted) and
/// `-0.0 == 0.0` (so a lost sign would pass unnoticed).
fn values_identical(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float4(x), Value::Float4(y)) => (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits(),
        (Value::Float8(x), Value::Float8(y)) => (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(l, r)| values_identical(l, r))
        }
        (Value::Vector(x), Value::Vector(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(l, r)| (l.is_nan() && r.is_nan()) || l.to_bits() == r.to_bits())
        }
        _ => a == b,
    }
}

fn rows_of(db: &EmbeddedDatabase, sql: &str, label: &str) -> Vec<Tuple> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{label} query failed: {sql}\n  {e}"))
}

/// Every row of `sql` must come back identical from source and restored.
fn assert_rows_match(rt: &RoundTrip, sql: &str) {
    let expected = rows_of(&rt.source, sql, "source");
    let actual = rows_of(&rt.restored, sql, "restored");
    assert_eq!(
        expected.len(),
        actual.len(),
        "row count differs for `{sql}`\n----- dump -----\n{}",
        rt.script
    );
    for (index, (want, got)) in expected.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            want.values.len(),
            got.values.len(),
            "row {index} arity differs for `{sql}`\n----- dump -----\n{}",
            rt.script
        );
        for (col, (w, g)) in want.values.iter().zip(got.values.iter()).enumerate() {
            assert!(
                values_identical(w, g),
                "row {index} column {col} differs for `{sql}`:\n  source   = {w:?}\n  restored = {g:?}\n\
                 ----- dump -----\n{}",
                rt.script
            );
        }
    }
    assert!(!expected.is_empty(), "`{sql}` returned no rows — vacuous comparison");
}

/// The catalog metadata the dump is supposed to carry: name, type, nullability
/// and default, for every column of `table`.
fn assert_columns_match(rt: &RoundTrip, table: &str) {
    let sql = format!(
        "SELECT column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns WHERE table_name = '{}' ORDER BY column_name",
        table.replace('\'', "''")
    );
    let expected = rows_of(&rt.source, &sql, "source");
    let actual = rows_of(&rt.restored, &sql, "restored");
    assert!(
        !expected.is_empty(),
        "information_schema.columns knows nothing about `{table}` — vacuous comparison"
    );
    assert_eq!(
        expected.len(),
        actual.len(),
        "column count differs for `{table}`\n----- dump -----\n{}",
        rt.script
    );
    for (index, (want, got)) in expected.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            want.values, got.values,
            "information_schema row {index} differs for `{table}`\n----- dump -----\n{}",
            rt.script
        );
    }
}

/// No `Value`/`DataType` reached the file through its `Debug` impl.
fn assert_no_debug_rendering(script: &str) {
    for marker in [
        "Json(",
        "Numeric(",
        "Uuid(",
        "Array(",
        "Vector(",
        "Bytes(",
        "DictRef",
        "CasRef",
        "ColumnarRef",
        "Varchar(Some",
        "Row data would go here",
        // `LogicalExpr::to_default_sql` debug-printed 15 of its 24 variants,
        // and CHECK bodies / column DEFAULTs are rendered from exactly that
        // enum — so the dump could contain a Rust struct literal inside an
        // otherwise-valid `CHECK (…)`.
        "Case {",
        "BinaryExpr {",
        "Column {",
        "InSet {",
        "ScalarFunction {",
    ] {
        assert!(
            !script.contains(marker),
            "dump leaked a Rust debug rendering ({marker}):\n{script}"
        );
    }
}

fn err_text(db: &EmbeddedDatabase, sql: &str) -> String {
    match db.execute(sql) {
        Ok(_) => panic!("expected `{sql}` to be rejected, but it succeeded"),
        Err(e) => e.to_string(),
    }
}

// ===========================================================================
// 1. The reported proof of concept
// ===========================================================================

#[test]
fn hdb005_report_poc_round_trips() {
    let rt = round_trip(&[
        r#"CREATE TABLE "my table" ("id" INT PRIMARY KEY, "payload" JSONB, "amount" NUMERIC, "ref" UUID, "tags" TEXT[])"#,
        r#"INSERT INTO "my table" VALUES (1, '["x"]', '3.14159', '2f4a1b6e-0000-4000-8000-000000000001', '{"p","q"}')"#,
    ]);

    // The headline defect: the identifier is quoted, so the statement parses.
    assert!(
        rt.script.contains(r#"CREATE TABLE IF NOT EXISTS "my table""#),
        "unquoted identifier is back:\n{}",
        rt.script
    );
    assert!(
        !rt.script.contains("CREATE TABLE IF NOT EXISTS my table"),
        "unquoted identifier is back:\n{}",
        rt.script
    );
    assert_no_debug_rendering(&rt.script);

    assert_rows_match(&rt, r#"SELECT * FROM "my table" ORDER BY "id""#);
    assert_columns_match(&rt, "my table");

    // And the values are the values, not their debug text.
    let rows = rows_of(&rt.restored, r#"SELECT * FROM "my table""#, "restored");
    assert_eq!(rows[0].values[1], Value::Json("[\"x\"]".to_string()));
    assert_eq!(rows[0].values[2], Value::Numeric("3.14159".to_string()));
    assert_eq!(
        rows[0].values[4],
        Value::Array(vec![Value::String("p".to_string()), Value::String("q".to_string())])
    );
}

// ===========================================================================
// 2. Identifiers that must be quoted
// ===========================================================================

#[test]
fn hdb005_identifiers_with_spaces_quotes_and_reserved_words() {
    let rt = round_trip(&[
        r#"CREATE TABLE "select" ("order" INT PRIMARY KEY, "a b" TEXT, "q""uote" TEXT)"#,
        r#"INSERT INTO "select" ("order", "a b", "q""uote") VALUES (1, 'spaced', 'quoted')"#,
        r#"INSERT INTO "select" ("order", "a b", "q""uote") VALUES (2, 'x', 'y')"#,
    ]);

    assert!(
        rt.script.contains(r#"CREATE TABLE IF NOT EXISTS "select""#),
        "reserved word not quoted:\n{}",
        rt.script
    );
    assert!(
        rt.script.contains(r#""a b""#) && rt.script.contains(r#""q""uote""#),
        "column identifiers not quoted:\n{}",
        rt.script
    );

    assert_rows_match(&rt, r#"SELECT * FROM "select" ORDER BY "order""#);
    assert_columns_match(&rt, "select");
}

// ===========================================================================
// 3. Strings
// ===========================================================================

#[test]
fn hdb005_string_edge_cases() {
    let rt = round_trip(&[
        "CREATE TABLE hdb005_str (id INT PRIMARY KEY, v TEXT)",
        "INSERT INTO hdb005_str VALUES (1, 'O''Brien')",
        r"INSERT INTO hdb005_str VALUES (2, 'back\slash')",
        "INSERT INTO hdb005_str VALUES (3, E'line1\nline2')",
        "INSERT INTO hdb005_str VALUES (4, E'tab\there')",
        "INSERT INTO hdb005_str VALUES (5, '日本語 café 🎉')",
        "INSERT INTO hdb005_str VALUES (6, '')",
        "INSERT INTO hdb005_str VALUES (7, NULL)",
        r"INSERT INTO hdb005_str VALUES (8, E'mixed: it''s\ta\\b\nend')",
    ]);

    // Every statement stays on one line, which is what the `--` comment lines
    // in the file rely on and what makes the dump greppable.
    for line in rt.script.lines() {
        assert!(
            !(line.starts_with("INSERT") && line.contains('\t')),
            "a raw tab escaped into the script: {line:?}"
        );
    }

    assert_rows_match(&rt, "SELECT * FROM hdb005_str ORDER BY id");
    assert_columns_match(&rt, "hdb005_str");

    let rows = rows_of(&rt.restored, "SELECT v FROM hdb005_str ORDER BY id", "restored");
    assert_eq!(rows[0].values[0], Value::String("O'Brien".to_string()));
    assert_eq!(rows[1].values[0], Value::String("back\\slash".to_string()));
    assert_eq!(rows[2].values[0], Value::String("line1\nline2".to_string()));
    assert_eq!(rows[3].values[0], Value::String("tab\there".to_string()));
    assert_eq!(rows[4].values[0], Value::String("日本語 café 🎉".to_string()));
    assert_eq!(rows[5].values[0], Value::String(String::new()));
    assert_eq!(rows[6].values[0], Value::Null);
}

// ===========================================================================
// 4. NUMERIC precision and the special floats
// ===========================================================================

#[test]
fn hdb005_numeric_precision_and_special_floats() {
    // 25 significant digits — well past what f64 can carry, and the reason
    // NUMERIC is always emitted as `'…'::numeric` rather than as a bare
    // decimal (a bare one re-parses through `number_literal_to_value`, i.e.
    // through f64).
    const BIG: &str = "1234567890123456789.012345";

    let rt = round_trip(&[
        "CREATE TABLE hdb005_num (id INT PRIMARY KEY, n NUMERIC, f FLOAT8)",
        &format!("INSERT INTO hdb005_num VALUES (1, '{BIG}', '1e300')"),
        "INSERT INTO hdb005_num VALUES (2, '-0.000000000000000000000001', '-0.0')",
        "INSERT INTO hdb005_num VALUES (3, 'NaN', 'NaN')",
        "INSERT INTO hdb005_num VALUES (4, 'Infinity', 'Infinity')",
        "INSERT INTO hdb005_num VALUES (5, '-Infinity', '-Infinity')",
    ]);

    assert_rows_match(&rt, "SELECT * FROM hdb005_num ORDER BY id");
    assert_columns_match(&rt, "hdb005_num");

    let rows = rows_of(&rt.restored, "SELECT n, f FROM hdb005_num ORDER BY id", "restored");
    // The NUMERIC payload is the SAME TEXT, digit for digit.
    assert_eq!(rows[0].values[0], Value::Numeric(BIG.to_string()));
    assert_eq!(rows[0].values[1], Value::Float8(1e300));

    // -0.0 keeps its sign — `Value::Float8(-0.0) == Value::Float8(0.0)` under
    // PartialEq, so this has to be asserted on the bits.
    match &rows[1].values[1] {
        Value::Float8(f) => {
            assert!(*f == 0.0 && f.is_sign_negative(), "lost the sign of -0.0: {f:?}");
        }
        other => panic!("expected FLOAT8, got {other:?}"),
    }

    assert_eq!(rows[2].values[0], Value::Numeric("NaN".to_string()));
    assert!(matches!(&rows[2].values[1], Value::Float8(f) if f.is_nan()));
    assert_eq!(rows[3].values[0], Value::Numeric("Infinity".to_string()));
    assert!(matches!(&rows[3].values[1], Value::Float8(f) if *f == f64::INFINITY));
    assert_eq!(rows[4].values[0], Value::Numeric("-Infinity".to_string()));
    assert!(matches!(&rows[4].values[1], Value::Float8(f) if *f == f64::NEG_INFINITY));
}

// ===========================================================================
// 5. Temporal types
// ===========================================================================

#[test]
fn hdb005_temporal_types() {
    let rt = round_trip(&[
        "CREATE TABLE hdb005_time (id INT PRIMARY KEY, d DATE, t TIME, ts TIMESTAMP, tz TIMESTAMPTZ)",
        "INSERT INTO hdb005_time VALUES (1, '2026-08-16', '01:11:00.123456', \
         '2026-08-16 01:11:00.123456', '2026-08-16 03:11:00.123456+02:00')",
        "INSERT INTO hdb005_time VALUES (2, '1970-01-01', '00:00:00', \
         '1970-01-01 00:00:00', '1970-01-01 00:00:00+00:00')",
        "INSERT INTO hdb005_time VALUES (3, NULL, NULL, NULL, NULL)",
        // FIX 3: NANOSECONDS. The engine stores `NaiveTime` / `DateTime<Utc>`
        // and reads the fraction back with chrono's `%.f`, which takes all
        // nine digits — a microsecond-clamped rendering (`%.6f` /
        // `SecondsFormat::Micros`) restored a DIFFERENT value than it dumped.
        "INSERT INTO hdb005_time VALUES (4, '2026-08-16', '01:11:00.123456789', \
         '2026-08-16 01:11:00.123456789', '2026-08-16 01:11:00.123456789+00:00')",
    ]);

    // Non-vacuity: the SOURCE really does hold nine digits, so comparing the
    // two databases is comparing something.
    let source_nanos = rows_of(&rt.source, "SELECT t FROM hdb005_time WHERE id = 4", "source");
    match &source_nanos[0].values[0] {
        Value::Time(t) => assert_eq!(
            chrono::Timelike::nanosecond(t),
            123_456_789,
            "the engine did not store nanoseconds; this test is vacuous"
        ),
        other => panic!("expected TIME, got {other:?}"),
    }
    assert!(
        rt.script.contains(".123456789"),
        "the dump clamped the fraction:\n{}",
        rt.script
    );

    assert_rows_match(&rt, "SELECT * FROM hdb005_time ORDER BY id");
    assert_columns_match(&rt, "hdb005_time");

    // The zone offset is honoured on the way in and on the way back: both rows
    // denote 01:11 UTC.
    let source = rows_of(&rt.source, "SELECT tz FROM hdb005_time WHERE id = 1", "source");
    let restored = rows_of(&rt.restored, "SELECT tz FROM hdb005_time WHERE id = 1", "restored");
    assert_eq!(source[0].values[0], restored[0].values[0]);
}

/// The INTERVAL gap, pinned so a future `cast_value` arm turns this test red
/// and the exclusion above can be removed. The serializer renders the literal
/// correctly; the engine cannot store it in a column.
#[test]
fn hdb005_interval_literal_is_still_column_less() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    // The expression form works — this is the rendering `sql_text` emits.
    let rows = db.query("SELECT INTERVAL '5 microseconds'", &[]).unwrap();
    assert_eq!(rows[0].values[0], Value::Interval(5));

    // The column form does not, on any input spelling.
    db.execute("CREATE TABLE hdb005_iv (id INT PRIMARY KEY, iv INTERVAL)")
        .unwrap();
    assert!(
        db.execute("INSERT INTO hdb005_iv VALUES (1, INTERVAL '5 microseconds')")
            .is_err(),
        "INTERVAL columns became writable — remove the exclusion in hdb005_temporal_types"
    );
}

// ===========================================================================
// 6. Arrays, JSON, BYTEA, VECTOR
// ===========================================================================

#[test]
fn hdb005_arrays_json_bytea_vector() {
    let rt = round_trip(&[
        "CREATE TABLE hdb005_wide (id INT PRIMARY KEY, ints INT[], texts TEXT[], \
         j JSON, jb JSONB, b BYTEA, v VECTOR(3))",
        r#"INSERT INTO hdb005_wide VALUES (1, '{1,2,3}', '{"a,b","",NULL}', '{"k":[1,2]}', '{"k":[1,2]}', '\x00ff00', '[1,2,3]')"#,
        r#"INSERT INTO hdb005_wide VALUES (2, '{}', '{}', '[]', '[]', '\x', '[0,-1.5,2.25]')"#,
        r#"INSERT INTO hdb005_wide VALUES (3, '{1,NULL,3}', '{"it''s","back\\slash"}', 'null', 'null', '\x0a0d09', '[0,0,0]')"#,
        "INSERT INTO hdb005_wide VALUES (4, NULL, NULL, NULL, NULL, NULL, NULL)",
    ]);

    assert_rows_match(&rt, "SELECT * FROM hdb005_wide ORDER BY id");
    assert_columns_match(&rt, "hdb005_wide");

    let rows = rows_of(
        &rt.restored,
        "SELECT ints, texts, b, v FROM hdb005_wide ORDER BY id",
        "restored",
    );
    assert_eq!(
        rows[0].values[0],
        Value::Array(vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)])
    );
    assert_eq!(
        rows[0].values[1],
        Value::Array(vec![
            Value::String("a,b".to_string()),
            Value::String(String::new()),
            Value::Null,
        ])
    );
    assert_eq!(rows[0].values[2], Value::Bytes(vec![0x00, 0xff, 0x00]));
    assert_eq!(rows[0].values[3], Value::Vector(vec![1.0, 2.0, 3.0]));
    // Empty array and empty bytea are values, not NULL.
    assert_eq!(rows[1].values[0], Value::Array(vec![]));
    assert_eq!(rows[1].values[2], Value::Bytes(vec![]));
    // A NULL member inside an array survives.
    assert_eq!(
        rows[2].values[0],
        Value::Array(vec![Value::Int4(1), Value::Null, Value::Int4(3)])
    );
}

// ===========================================================================
// 7. DDL metadata: defaults, NOT NULL, UNIQUE, composite PK, CHECK, FK
// ===========================================================================

#[test]
fn hdb005_ddl_metadata_survives() {
    // `child` sorts BEFORE `parent`, so the dump emits the child's CREATE TABLE
    // first — the ordering that makes an inline REFERENCES clause unloadable
    // and the reason foreign keys are written as trailing ALTER statements.
    const CHILD_DDL: &str = concat!(
        r#"CREATE TABLE child ("a" INT, "b" INT, "pid" INT NOT NULL, "#,
        r#""qty" INT NOT NULL DEFAULT 7, "created" TIMESTAMPTZ DEFAULT now(), "#,
        r#"PRIMARY KEY ("a", "b"), "#,
        r#"CONSTRAINT child_qty_check CHECK ("qty" > 0), "#,
        r#"CONSTRAINT child_pid_fk FOREIGN KEY ("pid") REFERENCES parent ("id"))"#,
    );

    let rt = round_trip(&[
        r#"CREATE TABLE parent ("id" INT PRIMARY KEY, "code" TEXT UNIQUE)"#,
        CHILD_DDL,
        r#"INSERT INTO parent ("id", "code") VALUES (1, 'alpha')"#,
        r#"INSERT INTO child ("a", "b", "pid") VALUES (1, 1, 1)"#,
    ]);

    // Foreign keys are a trailing ALTER, never an inline REFERENCES.
    assert!(
        rt.script.contains("ALTER TABLE \"child\" ADD CONSTRAINT")
            && rt
                .script
                .contains("FOREIGN KEY (\"pid\") REFERENCES \"parent\" (\"id\")"),
        "foreign key missing from the dump:\n{}",
        rt.script
    );
    let alter_at = rt.script.find("ALTER TABLE \"child\"").expect("FK ALTER present");
    let insert_at = rt.script.find("INSERT INTO \"child\"").expect("child INSERT present");
    assert!(
        insert_at < alter_at,
        "data must be loaded before the foreign key is added:\n{}",
        rt.script
    );
    assert!(
        rt.script.contains("PRIMARY KEY (\"a\", \"b\")"),
        "composite primary key missing:\n{}",
        rt.script
    );
    assert!(
        rt.script.contains("CHECK") && rt.script.contains("DEFAULT 7") && rt.script.contains("DEFAULT now()"),
        "CHECK / DEFAULTs missing:\n{}",
        rt.script
    );

    assert_rows_match(&rt, r#"SELECT "a", "b", "pid", "qty" FROM child ORDER BY "a", "b""#);
    assert_columns_match(&rt, "child");
    assert_columns_match(&rt, "parent");

    // Every constraint is ENFORCED in the restored database.
    let db = &rt.restored;

    // DEFAULT fills in.
    db.execute(r#"INSERT INTO child ("a", "b", "pid") VALUES (2, 2, 1)"#)
        .expect("default-filled insert");
    let rows = rows_of(db, r#"SELECT "qty" FROM child WHERE "a" = 2 AND "b" = 2"#, "restored");
    assert_eq!(rows[0].values[0], Value::Int4(7));

    // Composite PRIMARY KEY.
    let e = err_text(db, r#"INSERT INTO child ("a", "b", "pid") VALUES (1, 1, 1)"#).to_lowercase();
    assert!(
        e.contains("duplicate") || e.contains("unique") || e.contains("primary"),
        "composite PK not enforced: {e}"
    );

    // NOT NULL.
    let e = err_text(db, r#"INSERT INTO child ("a", "b", "pid") VALUES (3, 3, NULL)"#);
    assert!(e.to_lowercase().contains("null"), "NOT NULL not enforced: {e}");

    // CHECK.
    let e = err_text(db, r#"INSERT INTO child ("a", "b", "pid", "qty") VALUES (4, 4, 1, 0)"#);
    assert!(e.to_lowercase().contains("check"), "CHECK not enforced: {e}");

    // FOREIGN KEY.
    let e = err_text(db, r#"INSERT INTO child ("a", "b", "pid") VALUES (5, 5, 999)"#);
    assert!(
        e.to_lowercase().contains("foreign") || e.to_lowercase().contains("violat"),
        "FOREIGN KEY not enforced: {e}"
    );

    // UNIQUE on the parent.
    let e = err_text(db, r#"INSERT INTO parent ("id", "code") VALUES (2, 'alpha')"#);
    assert!(
        e.to_lowercase().contains("unique") || e.to_lowercase().contains("duplicate"),
        "UNIQUE not enforced: {e}"
    );
}

// ===========================================================================
// 8. The REPL uses the shared serializer
// ===========================================================================

#[test]
fn hdb005_repl_dump_uses_the_shared_serializer() {
    // `MetaCommand` and `MetaCommand::execute` are both public, so the meta
    // command is driven IN PROCESS here rather than source-grepped.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(r#"CREATE TABLE "my table" ("id" INT PRIMARY KEY, "v" VARCHAR(50))"#)
        .unwrap();
    db.execute(r#"INSERT INTO "my table" VALUES (1, 'alpha')"#).unwrap();

    let dir = TempDir::new().unwrap();
    // `\dump` splits its argument on whitespace, so the path must not contain any.
    let path = dir.path().join("repl_dump.sql");
    let command = MetaCommand::parse(&format!("\\dump {}", path.display())).expect("\\dump parses");
    command.execute(&db, false, None).expect("\\dump executes");

    let content = fs::read_to_string(&path).expect("REPL wrote the dump file");
    assert!(
        content.contains(r#"CREATE TABLE IF NOT EXISTS "my table""#),
        "REPL dump is not the shared format:\n{content}"
    );
    // The REPL's own two extra defects: the debug-printed column TYPE and the
    // row placeholder.
    assert!(
        content.contains("VARCHAR(50)"),
        "REPL still debug-prints the column type:\n{content}"
    );
    assert!(
        !content.contains("Row data would go here"),
        "REPL still writes row placeholders:\n{content}"
    );
    assert!(
        content.contains("VALUES (1, 'alpha')"),
        "REPL dump has no data:\n{content}"
    );

    // And it restores.
    let restored = EmbeddedDatabase::new_in_memory().unwrap();
    restored.execute_sql_script(&content).expect("REPL dump restores");
    let rows = rows_of(&restored, r#"SELECT "v" FROM "my table""#, "restored");
    assert_eq!(rows[0].values[0], Value::String("alpha".to_string()));
}

// ===========================================================================
// 9. Dictionary-encoded column
// ===========================================================================

#[test]
fn hdb005_dictionary_column_is_exported_resolved() {
    let rt = round_trip(&[
        "CREATE TABLE hdb005_tagged (id INT PRIMARY KEY, tag TEXT STORAGE DICTIONARY)",
        "INSERT INTO hdb005_tagged VALUES (1, 'alpha')",
        "INSERT INTO hdb005_tagged VALUES (2, 'alpha')",
        "INSERT INTO hdb005_tagged VALUES (3, 'beta')",
    ]);

    assert!(
        rt.script.contains("'alpha'") && rt.script.contains("'beta'"),
        "dictionary values were not materialised:\n{}",
        rt.script
    );
    for marker in ["dict:", "DictRef", "CasRef", "ColumnarRef"] {
        assert!(
            !rt.script.contains(marker),
            "storage reference leaked into the dump ({marker}):\n{}",
            rt.script
        );
    }

    assert_rows_match(&rt, "SELECT * FROM hdb005_tagged ORDER BY id");
}

// ===========================================================================
// 10. The script runner names the statement that failed
// ===========================================================================

#[test]
fn hdb005_script_runner_reports_the_failing_statement() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    let good = "CREATE TABLE hdb005_script (id INT PRIMARY KEY, v TEXT);\n\
                INSERT INTO hdb005_script VALUES (1, 'a');\n\
                INSERT INTO hdb005_script VALUES (2, 'b');";
    assert_eq!(db.execute_sql_script(good).expect("valid script"), 3);

    let bad = "INSERT INTO hdb005_script VALUES (3, 'c');\n\
               INSERT INTO hdb005_script VALUES (4, 'd');\n\
               INSERT INTO hdb005_script_does_not_exist VALUES (5, 'e');\n\
               INSERT INTO hdb005_script VALUES (6, 'f');";
    let err = db.execute_sql_script(bad).expect_err("3rd statement is invalid");
    let text = err.to_string();
    assert!(
        text.contains("statement 3"),
        "error does not name the statement: {text}"
    );

    // The first two took effect; the fourth never ran.
    let rows = rows_of(&db, "SELECT id FROM hdb005_script ORDER BY id", "db");
    let ids: Vec<Value> = rows.iter().map(|r| r.values[0].clone()).collect();
    assert_eq!(
        ids,
        vec![Value::Int4(1), Value::Int4(2), Value::Int4(3), Value::Int4(4)]
    );
}

// ===========================================================================
// 11. Comments and blank lines in a script are not statements
// ===========================================================================

#[test]
fn hdb005_script_runner_skips_comments_and_blank_segments() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let script = "-- HeliosDB Nano Database Dump\n\
                  -- Generated: whenever\n\
                  \n\
                  -- Table: t\n\
                  CREATE TABLE hdb005_cmt (id INT PRIMARY KEY);\n\
                  \n\
                  -- Data: t\n\
                  INSERT INTO hdb005_cmt VALUES (1);\n\
                  -- trailing comment\n";
    assert_eq!(db.execute_sql_script(script).expect("script runs"), 2);
    let rows = rows_of(&db, "SELECT id FROM hdb005_cmt", "db");
    assert_eq!(rows[0].values[0], Value::Int4(1));
}

// ===========================================================================
// 12. CHECK bodies quote their identifiers (the review's BLOCK)
// ===========================================================================

/// A CHECK on a column whose name needs quoting used to be written BARE:
/// `CHECK ((a b > 0))` — a syntax error in the FIRST statement of the file, so
/// the whole dump restored nothing — and `CHECK ((createdAt > …))`, which does
/// parse but re-binds on restore to a lower-cased column that does not exist,
/// leaving the constraint enforced by nothing.
///
/// `LogicalExpr::to_default_sql` (the `information_schema` readback renderer)
/// still writes them bare; the dump goes through `to_dump_sql`.
#[test]
fn hdb005_check_bodies_quote_their_identifiers() {
    const DDL: &str = concat!(
        r#"CREATE TABLE "hdb005 chk" ("id" INT PRIMARY KEY, "a b" INT, "createdAt" TIMESTAMPTZ, "#,
        r#"CONSTRAINT "My Chk" CHECK ("a b" > 0), "#,
        r#"CONSTRAINT camel_chk CHECK ("createdAt" IS NOT NULL))"#,
    );
    let rt = round_trip(&[
        DDL,
        r#"INSERT INTO "hdb005 chk" VALUES (1, 5, '2026-08-16 00:00:00+00:00')"#,
    ]);

    assert!(
        rt.script.contains(r#"CHECK (("a b" > 0))"#),
        "the CHECK body is still unquoted:\n{}",
        rt.script
    );
    assert!(
        rt.script.contains(r#"CHECK (("createdAt" IS NOT NULL))"#),
        "the camelCase column is unquoted in the CHECK body:\n{}",
        rt.script
    );
    assert_no_debug_rendering(&rt.script);

    assert_rows_match(&rt, r#"SELECT * FROM "hdb005 chk" ORDER BY "id""#);
    assert_columns_match(&rt, "hdb005 chk");

    // Non-vacuity: the SOURCE enforces it, so asserting the same on the
    // restored database is asserting that the constraint survived.
    let violating = r#"INSERT INTO "hdb005 chk" VALUES (2, 0, '2026-08-16 00:00:00+00:00')"#;
    assert!(
        err_text(&rt.source, violating).to_lowercase().contains("check"),
        "the source does not enforce the CHECK; this test would be vacuous"
    );
    let e = err_text(&rt.restored, violating);
    assert!(
        e.to_lowercase().contains("check"),
        "the restored CHECK is enforced by nothing: {e}"
    );
}

/// The implicit enum CHECK — `planner.rs` builds an `InList` over the type's
/// labels for every `"userRole" user_role` column, which is the Prisma /
/// Drizzle schema shape and the one that made the unquoted rendering reachable
/// without anyone writing a CHECK by hand.
#[test]
fn hdb005_enum_check_shape_round_trips() {
    let rt = round_trip(&[
        "CREATE TYPE user_role AS ENUM ('admin', 'user')",
        r#"CREATE TABLE hdb005_enum ("id" INT PRIMARY KEY, "userRole" user_role)"#,
        r#"INSERT INTO hdb005_enum VALUES (1, 'admin')"#,
    ]);

    assert!(
        rt.script.contains(r#"IN ('admin', 'user')"#) && rt.script.contains(r#""userRole""#),
        "the enum CHECK is missing or unquoted:\n{}",
        rt.script
    );
    assert_rows_match(&rt, "SELECT * FROM hdb005_enum ORDER BY \"id\"");
    assert_columns_match(&rt, "hdb005_enum");

    let bad = r#"INSERT INTO hdb005_enum VALUES (2, 'nope')"#;
    assert!(
        err_text(&rt.source, bad).to_lowercase().contains("check"),
        "the source does not enforce the enum CHECK; this test would be vacuous"
    );
    let e = err_text(&rt.restored, bad);
    assert!(e.to_lowercase().contains("check"), "the enum CHECK was lost: {e}");
}

/// A CHECK the exporter cannot spell FAILS the export, naming the constraint —
/// rather than writing `CHECK (Case { operand: None, … })` into a file that
/// will not load.
#[test]
fn hdb005_unrenderable_check_fails_the_export_by_name() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(
        "CREATE TABLE hdb005_case (id INT PRIMARY KEY, a INT, \
         CONSTRAINT hdb005_case_chk CHECK (CASE WHEN a > 0 THEN true ELSE false END))",
    )
    .expect("CASE CHECK is accepted by the engine");

    // Non-vacuity: the constraint really is RECORDED (a CHECK the planner
    // cannot convert is dropped silently, and then there would be nothing for
    // the exporter to choke on).
    let recorded = rows_of(
        &db,
        "SELECT constraint_name FROM information_schema.table_constraints \
         WHERE table_name = 'hdb005_case' AND constraint_type = 'CHECK'",
        "db",
    );
    assert_eq!(
        recorded.len(),
        1,
        "the CASE CHECK was not recorded, so this test would be vacuous: {recorded:?}"
    );

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("dump.sql");
    let err = db
        .dump_sql(&path)
        .expect_err("a CHECK the exporter cannot render must fail the export");
    let text = err.to_string();
    assert!(text.contains("hdb005_case_chk"), "the error does not name it: {text}");
    assert!(text.contains("CHECK constraint"), "{text}");
}

// ===========================================================================
// 13. Constraint names survive (FIX 2 + FIX 5)
// ===========================================================================

/// FK and CHECK constraint names used to be read back with `Ident::to_string()`,
/// which re-emits the quote characters: `CONSTRAINT "MyFk"` was STORED as
/// `"MyFk"` (quotes included), so every dump→restore cycle wrapped it in two
/// more — `"""MyFk"""`, `"""""""MyFk"""""""`, … — and
/// `information_schema.table_constraints` was already wrong after one.
#[test]
fn hdb005_quoted_constraint_names_survive_two_cycles() {
    let rt = round_trip(&[
        r#"CREATE TABLE hdb005_parent ("id" INT PRIMARY KEY)"#,
        concat!(
            r#"CREATE TABLE hdb005_child ("id" INT PRIMARY KEY, "pid" INT, "qty" INT, "#,
            r#"CONSTRAINT "My Chk" CHECK ("qty" > 0), "#,
            r#"CONSTRAINT "MyFk" FOREIGN KEY ("pid") REFERENCES hdb005_parent ("id"))"#,
        ),
        r#"INSERT INTO hdb005_parent VALUES (1)"#,
        r#"INSERT INTO hdb005_child VALUES (1, 1, 5)"#,
    ]);

    let names = "SELECT constraint_name, constraint_type FROM information_schema.table_constraints \
                 WHERE table_name = 'hdb005_child' ORDER BY constraint_name";
    let source_names = rows_of(&rt.source, names, "source");
    let restored_names = rows_of(&rt.restored, names, "restored");
    assert_eq!(
        source_names, restored_names,
        "constraint names did not survive the round trip\n----- dump -----\n{}",
        rt.script
    );
    let flattened = format!("{source_names:?}");
    assert!(flattened.contains("MyFk"), "the FK name is missing: {flattened}");
    assert!(flattened.contains("My Chk"), "the CHECK name is missing: {flattened}");
    assert!(
        !flattened.contains("\\\"MyFk"),
        "the stored name still carries its quote characters: {flattened}"
    );

    // Cycle two: the file the RESTORED database produces is byte-identical to
    // the one it was restored from (the header carries a timestamp, so only
    // the statement lines are compared). This is what fails when a name grows.
    let second = dump_script(&rt.restored, "second cycle");
    assert_eq!(
        statement_lines(&rt.script),
        statement_lines(&second),
        "the second dump differs from the first"
    );
}

/// FIX 5: a NAMED single-column UNIQUE used to be swallowed by the column's
/// `unique` flag and come back auto-named (`t_v_unique`), so
/// `information_schema.table_constraints` differed and a migration's
/// `ALTER TABLE … DROP CONSTRAINT my_uq` failed after a restore.
#[test]
fn hdb005_named_single_column_unique_keeps_its_name() {
    let rt = round_trip(&[
        r#"CREATE TABLE hdb005_uq ("id" INT PRIMARY KEY, "v" TEXT, CONSTRAINT my_uq UNIQUE ("v"))"#,
        r#"INSERT INTO hdb005_uq VALUES (1, 'alpha')"#,
    ]);

    assert!(
        rt.script.contains(r#"CONSTRAINT my_uq UNIQUE ("v")"#),
        "the named UNIQUE is not in the DDL:\n{}",
        rt.script
    );

    let names = "SELECT constraint_name FROM information_schema.table_constraints \
                 WHERE table_name = 'hdb005_uq' AND constraint_type = 'UNIQUE' ORDER BY constraint_name";
    assert_eq!(
        rows_of(&rt.source, names, "source"),
        rows_of(&rt.restored, names, "restored"),
        "UNIQUE constraint names differ\n----- dump -----\n{}",
        rt.script
    );

    // Still enforced, and droppable BY ITS NAME.
    let e = err_text(&rt.restored, r#"INSERT INTO hdb005_uq VALUES (2, 'alpha')"#);
    assert!(
        e.to_lowercase().contains("unique") || e.to_lowercase().contains("duplicate"),
        "UNIQUE not enforced after restore: {e}"
    );
    rt.restored
        .execute("ALTER TABLE hdb005_uq DROP CONSTRAINT my_uq")
        .expect("the constraint must still be reachable by its name");
}

// ===========================================================================
// 14. NOT ENFORCED foreign keys (FIX 4)
// ===========================================================================

/// `NOT ENFORCED` is the spelling people use precisely BECAUSE the data holds
/// dangling references. Dropping it on export re-imported the key as enforced,
/// so the restored database rejected writes the source accepted.
#[test]
fn hdb005_not_enforced_foreign_key_round_trips() {
    let rt = round_trip(&[
        "CREATE TABLE hdb005_fk_parent (id INT PRIMARY KEY)",
        "CREATE TABLE hdb005_fk_child (id INT PRIMARY KEY, pid INT, \
         CONSTRAINT hdb005_fk FOREIGN KEY (pid) REFERENCES hdb005_fk_parent (id) NOT ENFORCED)",
        // The dangling row the mode exists for.
        "INSERT INTO hdb005_fk_child VALUES (1, 999)",
    ]);

    assert!(
        rt.script.contains("NOT ENFORCED"),
        "the enforcement mode was dropped:\n{}",
        rt.script
    );
    assert_rows_match(&rt, "SELECT * FROM hdb005_fk_child ORDER BY id");

    // Non-vacuity: the source accepts another dangling row, so asserting the
    // same of the restored database asserts that the MODE came across.
    rt.source
        .execute("INSERT INTO hdb005_fk_child VALUES (2, 998)")
        .expect("source accepts a dangling row (NOT ENFORCED)");
    rt.restored
        .execute("INSERT INTO hdb005_fk_child VALUES (2, 998)")
        .expect("restored FK is enforced when the source's was not");
}
