//! `Value` → SQL-literal rendering: one renderer, every variant, both consumers.
//!
//! HeliosDB Nano substitutes values into SQL **textually** in two places, and until this
//! suite existed there were two independent implementations of the rendering — each broken
//! for exactly the types the other got right:
//!
//!   * `src/sql/interpolate.rs::value_to_sql_literal` — routine bodies (`LANGUAGE sql`
//!     functions and procedures, and the PL/pgSQL runtime). Its `Timestamp` arm rendered
//!     `format!("'{}'", ts)`, i.e. `Display for DateTime<Utc>`, which appends a timezone
//!     **name**: `'2026-08-16 01:11:00 UTC'`. Measured: `CALL put($1)` with a `Timestamp`
//!     argument failed with
//!     `Cannot cast '2026-08-16 01:11:00 UTC' to TIMESTAMP: trailing input`.
//!   * `src/protocol/postgres/prepared.rs::value_to_sql_literal` — a private fork used by
//!     `substitute_parameters` on the PostgreSQL extended-query path. Its catch-all arm
//!     was `format!("'{}'", value.to_string().replace('\'', "''"))`, routed through
//!     `impl Display for Value` (`src/types.rs`) — which ALREADY emits its own quotes for
//!     `Date` / `Time` / `Uuid` / `String` / `Json` / `Timestamp`. Everything that reached
//!     the catch-all came out **triple-quoted**. Measured:
//!     `Date` → `'''2026-08-16'''` → `Cannot cast ''2026-08-16'' to DATE`;
//!     `Time` → `'''01:11:00'''`; `Uuid` → `'''0000…'''`.
//!
//! Both are fixed, and the fork is deleted: `prepared.rs` now calls the shared renderer.
//! The renderer's contract is **"valid SQL that re-parses to an equal `Value`"**, which is
//! strictly stronger than what the extended-protocol consumer needs (its substituted text
//! is handed only to the regex-driven catalog dispatcher), so one implementation serves
//! both.
//!
//! HOW TO MAINTAIN THIS FILE. Every test asserts unconditionally — never introduce an
//! `is_ok()` guard, and never assert on a `SELECT COUNT(*)` row count (a count query
//! returns one row whether the count is 0 or 10,000); read the stored value back and
//! compare it. `cases()` (plus `uncolumned_cases()`) is the pinned per-variant table: if
//! you change a rendering you must change it there, and the round-trip tests must still
//! pass through BOTH consumers.

use heliosdb_nano::protocol::postgres::prepared::substitute_parameters;
use heliosdb_nano::sql::interpolate::value_to_sql_literal;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

fn uuid_fixture() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").expect("fixture uuid")
}

fn timestamp_fixture() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-08-16T01:11:00Z")
        .expect("fixture timestamp")
        .with_timezone(&chrono::Utc)
}

fn date_fixture() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2026, 8, 16).expect("fixture date")
}

fn time_fixture() -> chrono::NaiveTime {
    chrono::NaiveTime::from_hms_opt(1, 11, 0).expect("fixture time")
}

// ===========================================================================
// The pinned rendering table.
//
// One row per `Value` variant that a caller can produce. `column_type` is the SQL
// column type the value is stored into for the round-trip tests; `stored` is what
// reading that column back must yield (INSERT casts the parsed literal to the column
// type, so widening — Int2 rendered as `7` re-parses as Int4 and casts back to Int2 —
// is invisible here, which is exactly the property callers depend on).
// ===========================================================================

struct Case {
    /// Identifier-safe name; also used to build table/procedure names.
    name: &'static str,
    /// SQL type of the capture column and of the procedure's declared parameter.
    column_type: &'static str,
    /// The value handed to `CALL p($1)` / `substitute_parameters`.
    value: Value,
    /// The exact literal the renderer must emit.
    literal: String,
    /// What the capture column must hold afterwards.
    stored: Value,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "null",
            column_type: "INT",
            value: Value::Null,
            literal: "NULL".to_string(),
            stored: Value::Null,
        },
        Case {
            name: "boolean",
            column_type: "BOOLEAN",
            value: Value::Boolean(true),
            literal: "TRUE".to_string(),
            stored: Value::Boolean(true),
        },
        Case {
            name: "int2",
            column_type: "SMALLINT",
            value: Value::Int2(-7),
            literal: "-7".to_string(),
            stored: Value::Int2(-7),
        },
        Case {
            name: "int4",
            column_type: "INT",
            value: Value::Int4(42),
            literal: "42".to_string(),
            stored: Value::Int4(42),
        },
        Case {
            name: "int8",
            column_type: "BIGINT",
            value: Value::Int8(9_000_000_000),
            literal: "9000000000".to_string(),
            stored: Value::Int8(9_000_000_000),
        },
        Case {
            name: "float4",
            column_type: "REAL",
            value: Value::Float4(1.5),
            literal: "1.5".to_string(),
            stored: Value::Float4(1.5),
        },
        Case {
            name: "float8",
            column_type: "DOUBLE PRECISION",
            value: Value::Float8(2.25),
            literal: "2.25".to_string(),
            stored: Value::Float8(2.25),
        },
        Case {
            name: "numeric",
            column_type: "NUMERIC",
            value: Value::Numeric("12.34".to_string()),
            literal: "12.34".to_string(),
            stored: Value::Numeric("12.34".to_string()),
        },
        Case {
            // The quote-doubling case. `O'Brien` must arrive as data, not as a syntax
            // error and not as an injection point.
            name: "text",
            column_type: "TEXT",
            value: Value::String("O'Brien".to_string()),
            literal: "'O''Brien'".to_string(),
            stored: Value::String("O'Brien".to_string()),
        },
        Case {
            name: "bytes",
            column_type: "BYTEA",
            value: Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            literal: r"E'\\xdeadbeef'".to_string(),
            stored: Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        },
        Case {
            // REGRESSION: rendered `'''0000…'''` by the extended-protocol fork.
            name: "uuid",
            column_type: "UUID",
            value: Value::Uuid(uuid_fixture()),
            literal: "'00000000-0000-4000-8000-000000000001'".to_string(),
            stored: Value::Uuid(uuid_fixture()),
        },
        Case {
            // REGRESSION: rendered `'2026-08-16 01:11:00 UTC'` by the shared renderer.
            name: "timestamp",
            column_type: "TIMESTAMP",
            value: Value::Timestamp(timestamp_fixture()),
            literal: "'2026-08-16T01:11:00+00:00'".to_string(),
            stored: Value::Timestamp(timestamp_fixture()),
        },
        Case {
            // REGRESSION: rendered `'''2026-08-16'''` by the extended-protocol fork.
            name: "date",
            column_type: "DATE",
            value: Value::Date(date_fixture()),
            literal: "'2026-08-16'".to_string(),
            stored: Value::Date(date_fixture()),
        },
        Case {
            // REGRESSION: rendered `'''01:11:00'''` by the extended-protocol fork.
            name: "time",
            column_type: "TIME",
            value: Value::Time(time_fixture()),
            literal: "'01:11:00'".to_string(),
            stored: Value::Time(time_fixture()),
        },
        Case {
            name: "json",
            column_type: "JSONB",
            value: Value::Json(r#"{"a":1}"#.to_string()),
            literal: r#"'{"a":1}'"#.to_string(),
            stored: Value::Json(r#"{"a":1}"#.to_string()),
        },
        Case {
            name: "array",
            column_type: "INT[]",
            value: Value::Array(vec![Value::Int4(1), Value::Int4(2)]),
            literal: "ARRAY[1,2]".to_string(),
            stored: Value::Array(vec![Value::Int4(1), Value::Int4(2)]),
        },
        Case {
            // The fork rendered `ARRAY[1.5,2.5]`, which re-parses as an ARRAY; the shared
            // renderer rendered a bare `[1.5,2.5]`, which is not SQL at all. Neither
            // round-tripped. pgvector's text form does.
            name: "vector",
            column_type: "VECTOR(2)",
            value: Value::Vector(vec![1.5, 2.5]),
            literal: "'[1.5,2.5]'::vector".to_string(),
            stored: Value::Vector(vec![1.5, 2.5]),
        },
    ]
}

/// Variants that `cases()` cannot drive through a typed column, with the reason.
/// Their renderings are still pinned, and `Interval` still round-trips as an
/// expression (`interval_round_trips_as_an_expression`).
fn uncolumned_cases() -> Vec<(&'static str, Value, String)> {
    vec![
        // `CAST(… AS INTERVAL)` is unimplemented in the evaluator (`cast_value` has no
        // `DataType::Interval` arm), and INSERT casts every value to its column type, so
        // an INTERVAL column cannot be written at all. That is an engine gap, not a
        // rendering gap — the literal below parses and evaluates correctly.
        ("interval", Value::Interval(5), "INTERVAL '5 microseconds'".to_string()),
        // Storage-internal references. These are meant to be resolved before a value
        // reaches the renderer; the renderings are best-effort markers, and are pinned
        // only so a future change to them is deliberate.
        ("dict_ref", Value::DictRef { dict_id: 7 }, "'dict:7'".to_string()),
        (
            "cas_ref",
            Value::CasRef { hash: [0xab; 32] },
            format!("E'\\\\x{}'", "ab".repeat(32)),
        ),
        ("columnar_ref", Value::ColumnarRef, "NULL".to_string()),
    ]
}

// ===========================================================================
// 1. The rendering itself.
// ===========================================================================

#[test]
fn every_variant_renders_exactly_the_pinned_literal() {
    for case in cases() {
        assert_eq!(
            value_to_sql_literal(&case.value),
            case.literal,
            "rendering drifted for `{}`",
            case.name
        );
    }
    for (name, value, literal) in uncolumned_cases() {
        assert_eq!(value_to_sql_literal(&value), literal, "rendering drifted for `{name}`");
    }
}

#[test]
fn no_rendering_ever_produces_a_doubled_leading_quote() {
    // The SHAPE of the extended-protocol fork's bug: a catch-all that wrapped
    // `Display for Value` output in quotes, when Display already supplied them. Every
    // such value began `''` — `'''2026-08-16'''` — and every one of them failed to cast.
    // Asserted over every variant, through BOTH entry points, so neither can regress.
    fn check(name: &str, value: &Value, allow_escaped_quote: bool) {
        let direct = value_to_sql_literal(value);
        assert!(
            !direct.starts_with("''"),
            "`{name}` rendered a doubled leading quote: {direct}"
        );
        let substituted = substitute_parameters("SELECT $1", std::slice::from_ref(value))
            .unwrap_or_else(|e| panic!("substitute_parameters failed for `{name}`: {e}"));
        assert_eq!(substituted, format!("SELECT {direct}"));
        if !allow_escaped_quote {
            assert!(
                !substituted.contains("''"),
                "`{name}` substituted a doubled quote outside a value: {substituted}"
            );
        }
    }

    let mut checked = 0usize;
    for case in cases() {
        // `text` carries the one legitimate `''`: it is the ESCAPE for an embedded quote,
        // not a doubled delimiter. Its leading-quote shape is still asserted.
        check(case.name, &case.value, case.name == "text");
        checked += 1;
    }
    for (name, value, _) in uncolumned_cases() {
        check(name, &value, false);
        checked += 1;
    }
    assert_eq!(value_to_sql_literal(&Value::String("O'Brien".into())), "'O''Brien'");

    // Guard against the loop silently covering nothing.
    assert_eq!(
        checked,
        cases().len() + uncolumned_cases().len(),
        "not every variant was checked"
    );
}

#[test]
fn the_two_renderers_are_one_function() {
    // `substitute_parameters` must produce byte-identical output to the shared renderer
    // for every variant. If this fails, a fork has been reintroduced.
    for case in cases() {
        let substituted = substitute_parameters("SELECT $1", std::slice::from_ref(&case.value))
            .unwrap_or_else(|e| panic!("substitute_parameters failed for `{}`: {e}", case.name));
        assert_eq!(
            substituted,
            format!("SELECT {}", case.literal),
            "extended-protocol rendering diverged from the shared renderer for `{}`",
            case.name
        );
    }
}

// ===========================================================================
// 2. Round-trip through a `LANGUAGE sql` procedure body — `CALL p($1)` with
//    `execute_params`, the shape every extended-protocol driver emits.
// ===========================================================================

#[test]
fn every_variant_round_trips_through_a_sql_procedure_body() {
    for case in cases() {
        let db = db();
        let table = format!("vr_proc_{}", case.name);
        let proc = format!("vr_put_{}", case.name);

        db.execute(&format!("CREATE TABLE {table} (v {})", case.column_type))
            .unwrap_or_else(|e| panic!("CREATE TABLE for `{}` failed: {e}", case.name));
        db.execute(&format!(
            "CREATE PROCEDURE {proc}(p {}) LANGUAGE sql AS $$INSERT INTO {table} VALUES ($1)$$",
            case.column_type
        ))
        .unwrap_or_else(|e| panic!("CREATE PROCEDURE for `{}` failed: {e}", case.name));

        db.execute_params(&format!("CALL {proc}($1)"), std::slice::from_ref(&case.value))
            .unwrap_or_else(|e| {
                panic!(
                    "CALL {proc}($1) failed for `{}` (rendered `{}`): {e}",
                    case.name, case.literal
                )
            });

        let rows = db
            .query(&format!("SELECT v FROM {table}"), &[])
            .unwrap_or_else(|e| panic!("read-back for `{}` failed: {e}", case.name));
        assert_eq!(rows.len(), 1, "`{}` wrote {} rows", case.name, rows.len());
        assert_eq!(
            rows[0].values[0], case.stored,
            "`{}` did not survive the procedure body (rendered `{}`)",
            case.name, case.literal
        );
    }
}

#[test]
fn timestamp_argument_survives_a_procedure_body() {
    // THE measured defect-1 regression, on its own so its failure is unambiguous.
    // Before the fix: `Cannot cast '2026-08-16 01:11:00 UTC' to TIMESTAMP: trailing
    // input` — `Display for DateTime<Utc>` appended a timezone NAME.
    let db = db();
    db.execute("CREATE TABLE vr_ts (v TIMESTAMP)").expect("create");
    db.execute("CREATE PROCEDURE vr_put_ts(p TIMESTAMP) LANGUAGE sql AS $$INSERT INTO vr_ts VALUES ($1)$$")
        .expect("create procedure");

    db.execute_params("CALL vr_put_ts($1)", &[Value::Timestamp(timestamp_fixture())])
        .expect("a TIMESTAMP argument must reach the body as a castable literal");

    let rows = db.query("SELECT v FROM vr_ts", &[]).expect("read back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Timestamp(timestamp_fixture()));
}

// ===========================================================================
// 3. Round-trip through `substitute_parameters` followed by `execute` — the
//    PostgreSQL extended-query path's textual form.
// ===========================================================================

#[test]
fn every_variant_round_trips_through_substitute_parameters() {
    for case in cases() {
        let db = db();
        let table = format!("vr_sub_{}", case.name);
        db.execute(&format!("CREATE TABLE {table} (v {})", case.column_type))
            .unwrap_or_else(|e| panic!("CREATE TABLE for `{}` failed: {e}", case.name));

        let sql = substitute_parameters(
            &format!("INSERT INTO {table} VALUES ($1)"),
            std::slice::from_ref(&case.value),
        )
        .unwrap_or_else(|e| panic!("substitute_parameters failed for `{}`: {e}", case.name));

        db.execute(&sql)
            .unwrap_or_else(|e| panic!("executing `{sql}` for `{}` failed: {e}", case.name));

        let rows = db
            .query(&format!("SELECT v FROM {table}"), &[])
            .unwrap_or_else(|e| panic!("read-back for `{}` failed: {e}", case.name));
        assert_eq!(rows.len(), 1, "`{}` wrote {} rows", case.name, rows.len());
        assert_eq!(
            rows[0].values[0], case.stored,
            "`{}` did not survive substitution (rendered `{}`)",
            case.name, case.literal
        );
    }
}

#[test]
fn date_time_and_uuid_parameters_survive_substitute_parameters() {
    // THE measured defect-2 regressions, on their own. Before the fix the fork's
    // catch-all re-quoted `Display for Value`, so these three rendered
    // `'''2026-08-16'''` / `'''01:11:00'''` / `'''0000…'''` and every one failed to cast.
    let db = db();
    db.execute("CREATE TABLE vr_dtu (d DATE, t TIME, u UUID)")
        .expect("create");

    let sql = substitute_parameters(
        "INSERT INTO vr_dtu VALUES ($1, $2, $3)",
        &[
            Value::Date(date_fixture()),
            Value::Time(time_fixture()),
            Value::Uuid(uuid_fixture()),
        ],
    )
    .expect("substitution");
    assert_eq!(
        sql,
        "INSERT INTO vr_dtu VALUES ('2026-08-16', '01:11:00', '00000000-0000-4000-8000-000000000001')"
    );

    db.execute(&sql)
        .expect("DATE/TIME/UUID parameters must produce castable literals");

    let rows = db.query("SELECT d, t, u FROM vr_dtu", &[]).expect("read back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Date(date_fixture()));
    assert_eq!(rows[0].values[1], Value::Time(time_fixture()));
    assert_eq!(rows[0].values[2], Value::Uuid(uuid_fixture()));
}

// ===========================================================================
// 4. The variants that cannot go through a typed column.
// ===========================================================================

#[test]
fn interval_round_trips_as_an_expression() {
    // No INTERVAL column round-trip is possible — `cast_value` has no `DataType::Interval`
    // arm, so INSERT into an INTERVAL column fails regardless of rendering. The literal
    // itself parses and evaluates back to the same microsecond count, which is the part
    // this suite owns.
    let db = db();
    let literal = value_to_sql_literal(&Value::Interval(5));
    assert_eq!(literal, "INTERVAL '5 microseconds'");

    let rows = db
        .query(&format!("SELECT {literal}"), &[])
        .unwrap_or_else(|e| panic!("`SELECT {literal}` failed: {e}"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Interval(5));
}

// ===========================================================================
// 5. Properties the renderer must keep whatever else changes.
// ===========================================================================

#[test]
fn a_quote_bearing_value_cannot_break_out_of_its_literal() {
    // The injection route. Every string-ish arm doubles `'`; this asserts the doubling
    // survives substitution AND execution — the payload closes the literal, ends the
    // statement and drops the table if the doubling is ever lost.
    let db = db();
    db.execute("CREATE TABLE vr_inj (v TEXT)").expect("create");

    let hostile = "'); DROP TABLE vr_inj; --";
    let sql = substitute_parameters("INSERT INTO vr_inj VALUES ($1)", &[Value::String(hostile.to_string())])
        .expect("substitution");
    db.execute(&sql).expect("hostile text is data, not syntax");

    let rows = db.query("SELECT v FROM vr_inj", &[]).expect("table must still exist");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::String(hostile.to_string()));
}

#[test]
fn a_numeric_that_is_not_a_number_is_quoted_and_cast() {
    // `Value::Numeric` is String-backed and can hold PostgreSQL's special tokens. Bare
    // `NaN` would splice an identifier into SQL; the renderer quotes and casts instead.
    assert_eq!(
        value_to_sql_literal(&Value::Numeric("NaN".to_string())),
        "'NaN'::numeric"
    );
    assert_eq!(
        value_to_sql_literal(&Value::Numeric("Infinity".to_string())),
        "'Infinity'::numeric"
    );
    // A plain decimal stays unquoted so it keeps its numeric-ness.
    assert_eq!(value_to_sql_literal(&Value::Numeric("-0.75".to_string())), "-0.75");

    let db = db();
    db.execute("CREATE TABLE vr_num (v NUMERIC)").expect("create");
    let sql = substitute_parameters("INSERT INTO vr_num VALUES ($1)", &[Value::Numeric("NaN".to_string())])
        .expect("substitution");
    db.execute(&sql).expect("a NaN numeric must be executable");
    let rows = db.query("SELECT v FROM vr_num", &[]).expect("read back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Numeric("NaN".to_string()));
}

#[test]
fn multi_digit_placeholders_still_resolve_after_the_merge() {
    // `substitute_parameters` keeps its own scanner (it must handle `$10` and `$N::type`);
    // only the rendering moved. Pinned so the merge cannot have changed the scanner.
    let params: Vec<Value> = (1..=11).map(Value::Int4).collect();
    let out = substitute_parameters("VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)", &params).expect("substitution");
    assert_eq!(out, "VALUES (1,2,3,4,5,6,7,8,9,10,11)");

    let cast = substitute_parameters(
        "SELECT $1::text, $2::int",
        &[Value::String("hi".into()), Value::Int4(4)],
    )
    .expect("substitution");
    assert_eq!(cast, "SELECT 'hi'::text, 4::int");
}
