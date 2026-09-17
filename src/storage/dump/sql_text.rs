//! Schema-aware SQL **text** serialisation for `dump_sql` (HDB-005).
//!
//! ONE serializer, used by the library exporter (`DumpManager::create_sql_dump`)
//! and by the REPL's `\dump` meta-command. Before this module each of those had
//! its own hand-rolled copy, and both emitted SQL that could not be restored:
//!
//! * identifiers were written bare — `CREATE TABLE IF NOT EXISTS my table (`;
//! * values fell back to `format!("'{:?}'", value)`, so a JSON column exported
//!   as `'Json("[\"x\"]")'`, a NUMERIC as `'Numeric("3.14159")'`, a UUID as
//!   `'Uuid(…)'` and an array as `'Array([String("p")…])'`;
//! * the REPL additionally printed the COLUMN TYPE with `{:?}` (`Varchar(Some(50))`)
//!   and emitted `-- Row data would go here` instead of any data at all;
//! * defaults, constraints and indexes were not emitted.
//!
//! ## Contract
//!
//! Every statement this module produces is a single LINE and re-parses through
//! `EmbeddedDatabase::execute` into the value it came from. The literal forms
//! were chosen against the engine's own parsing rules rather than against
//! PostgreSQL's documentation:
//!
//! | `Value` | rendering | path it re-enters through |
//! |---|---|---|
//! | `Null` | `NULL` | — |
//! | `Boolean` | `TRUE` / `FALSE` | `sql_value_to_value` |
//! | `Int2/4/8` | `42` | `number_literal_to_value` (widens, then casts down) |
//! | `Float4/8` finite | `1.5` (`{:?}`, shortest round-trip) | `number_literal_to_value` |
//! | `Float4/8` non-finite | `'NaN'::float8` … | `cast_value` String→Float (`str::parse`) |
//! | `Numeric` | `'3.14159'::numeric` ALWAYS | `parse_numeric_text` (28-digit `Decimal`) |
//! | `String` | `'O''Brien'`, or `E'…'` when it holds `\ \n \r \t` | `sql_value_to_value` |
//! | `Bytes` | `E'\\x6162'::bytea` | `cast_value` String→Bytea (hex) |
//! | `Uuid` | `'0000…'::uuid` | `cast_value` String→Uuid |
//! | `Timestamp` | `'…+00:00'::timestamptz` / `'… '::timestamp`, 9 fractional digits | `parse_timestamp_literal` |
//! | `Date` | `'2026-08-16'::date` | `%Y-%m-%d` |
//! | `Time` | `'01:11:00.123456789'::time` | `%H:%M:%S%.f` |
//! | `Interval` | `INTERVAL '5 microseconds'` | `Planner::parse_interval_text` |
//! | `Json` | `'{"a":1}'::jsonb` | `cast_value` String→Json(b) |
//! | `Array` | `'{"a","b"}'::text[]` | `parse_pg_array_text_literal` |
//! | `Vector` | `'[1,2,3]'::vector(3)` | `sql_data_type_to_data_type` |
//! | `DictRef`/`CasRef`/`ColumnarRef` | **`Err`** | — never a placeholder |
//!
//! Two deliberate departures from the obvious spelling, both forced by the
//! engine:
//!
//! * **Arrays use the PostgreSQL array TEXT literal, not `ARRAY[…]`.** The
//!   planner's `ARRAY[…]` arm turns an all-numeric list that contains a decimal
//!   point into a `Value::Vector` (vector-search compatibility), so a
//!   `float8[]` column would round-trip as a vector and then fail its cast; and
//!   that arm only accepts bare `Number` / `SingleQuotedString` / `Boolean` /
//!   `Null` elements, so an element needing an `E'…'` escape has no spelling at
//!   all. The text form routes through `Evaluator::parse_pg_array_text_literal`,
//!   which quotes every element and casts each one to the declared element
//!   type.
//! * **`Numeric` is always quoted and cast.** A bare decimal re-parses through
//!   `f64` (`number_literal_to_value`) and silently loses everything past ~17
//!   significant digits; `'…'::numeric` goes to `Decimal` and keeps 28.
//!
//! Temporal values carry NINE fractional digits, not six. The engine stores
//! `NaiveTime` / `DateTime<Utc>`, which are nanosecond-resolution, and both
//! `parse_timestamp_literal` and the `TIME` cast arm read the fraction with
//! chrono's `%.f`, which accepts all nine — so a microsecond-clamped rendering
//! restored a DIFFERENT value than it dumped.
//!
//! ## What the exporter REFUSES to write
//!
//! CHECK bodies and column DEFAULTs are rendered by
//! [`crate::sql::LogicalExpr::to_dump_sql`], which quotes every identifier and
//! returns `Err` for every expression shape it cannot spell. The catalog's own
//! readback renderer (`to_default_sql`) does neither — it writes column
//! references bare and falls back to Rust `Debug` output — which is why
//! `CHECK ("a b" > 0)` used to be written as `CHECK ((a b > 0))`, the first
//! statement in the file and therefore a dump that restored NOTHING. Failing
//! the export, naming the constraint, is the only honest alternative.
//!
//! ## Known engine gap (NOT introduced here)
//!
//! `Evaluator::cast_value` has no `DataType::Interval` arm, and the INSERT
//! coercion gate casts every value whose variant is not on its short identity
//! list — so INSERT into an INTERVAL **column** fails whatever literal is
//! written (see `tests/value_rendering_tests.rs::interval_round_trips_as_an_expression`).
//! `value_literal` therefore renders the correct `INTERVAL '…'` form and the
//! round-trip test for it is scoped to the expression, not a column.

use crate::sql::{ConstraintEnforcement, ForeignKeyConstraint, LogicalExpr, ReferentialAction, TableConstraints};
use crate::{Column, DataType, Error, Result, Schema, Tuple, Value};

use super::manager::IndexMetadata;

/// Rows folded into one multi-row `INSERT … VALUES (…), (…)` statement.
pub(crate) const INSERT_BATCH_ROWS: usize = 100;

// ---------------------------------------------------------------------------
// Identifiers and literals
// ---------------------------------------------------------------------------

/// Double-quote an identifier, doubling any embedded `"`.
///
/// Unconditionally, even for a plain lower-case name: PostgreSQL (and Nano's
/// `Planner::normalize_ident`) treat a quoted identifier as written, and a
/// stored name is ALREADY in its final form — lower-cased if it was written
/// unquoted, preserved if it was not. Re-emitting it quoted is therefore the
/// only spelling that is right for both, and it is the only one that survives a
/// name with a space, a reserved word (`"select"`) or an embedded quote.
pub(crate) fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for ch in name.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Quote a catalog table key, which may be `schema.table`.
///
/// Splits at the FIRST `.`, exactly as `Catalog::list_tables_qualified` does,
/// so a table in a non-`public` schema round-trips as `"s"."t"` rather than as
/// one identifier literally named `s.t`.
pub(crate) fn quote_table_name(name: &str) -> String {
    match name.split_once('.') {
        Some((schema, table)) => format!("{}.{}", quote_ident(schema), quote_ident(table)),
        None => quote_ident(name),
    }
}

/// Spelling for a CONSTRAINT name.
///
/// Quoted only when it has to be, and then EXACTLY round-tripping: every
/// constraint-name reader in the planner now normalises through
/// `Planner::normalize_ident`, so a bare lower-case name comes back unchanged
/// and a quoted one comes back without its quote characters (HDB-005 FIX 2 —
/// the FK and CHECK arms used to read the name with `Ident::to_string()`, which
/// re-emits the quotes, so `CONSTRAINT "MyFk"` was stored as `\"MyFk\"` and
/// grew two more quote characters on every dump→restore cycle).
///
/// Auto-generated names (`fk_child_pid__parent`, `t_pkey`, `t_v_unique`) are
/// plain lower-case identifiers and are emitted bare.
pub(crate) fn quote_constraint_name(name: &str) -> String {
    let mut chars = name.chars();
    let head_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_');
    let tail_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if head_ok && tail_ok {
        name.to_string()
    } else {
        quote_ident(name)
    }
}

/// True when `s` needs the `E'…'` escape form to stay on one line.
fn needs_escape_string(s: &str) -> bool {
    // Plain `'…'` for everything else — including non-ASCII text, which the
    // `E'…'` path does NOT carry intact (the escape processor works on bytes
    // and Latin-1-expands multi-byte characters). The plain form round-trips
    // `日本語 café 🎉`; the residual mojibake-shaped input (`Ã©`) that the
    // planner's `repair_sqlparser_string` heuristic rewrites is a filed,
    // pre-existing planner issue, not something the exporter can route around.
    s.chars().any(|c| matches!(c, '\\' | '\n' | '\r' | '\t'))
}

/// A SQL string literal for `s`.
///
/// `E'…'` when the text holds a backslash, newline, carriage return or tab
/// (escaped `\\` / `\n` / `\r` / `\t`), otherwise a plain `'…'`. Either way `'`
/// is doubled — `''` rather than `\'` even inside `E'…'`, because the script
/// splitter recognises the doubled form explicitly.
///
/// The E-form is what keeps every emitted statement on ONE line, which is what
/// makes the dump greppable and its `-- …` comment lines unambiguous.
pub(crate) fn quote_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    if needs_escape_string(s) {
        out.push_str("E'");
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\'' => out.push_str("''"),
                other => out.push(other),
            }
        }
    } else {
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push('\'');
            }
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Strip everything from a `-- …` comment that could change how the script
/// splitter reads the rest of the file (`'` opens a literal, `;` ends a
/// statement, `$` can open a dollar-quote, a newline would split the comment).
pub(crate) fn sanitize_comment(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\'' | '"' | ';' | '$' | '\\' | '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// The SQL type name a float value casts through.
const fn float_cast_name(is_f32: bool) -> &'static str {
    if is_f32 {
        "float4"
    } else {
        "float8"
    }
}

/// Render one finite/non-finite float.
///
/// Finite floats use `{:?}` — Rust's shortest representation that round-trips,
/// which always carries a `.` or an `e` so `number_literal_to_value` types it
/// as a float rather than as an integer, and which spells large magnitudes
/// `1e300` rather than as 301 digits that `Decimal` could not hold. The cast is
/// appended only when the declared column type is a DIFFERENT float width, so
/// the common case stays a bare literal.
fn float_literal(value: f64, is_f32: bool, declared: &DataType) -> String {
    let cast = float_cast_name(is_f32);
    if value.is_nan() {
        return format!("'NaN'::{cast}");
    }
    if value.is_infinite() {
        let sign = if value.is_sign_negative() { "-" } else { "" };
        return format!("'{sign}Infinity'::{cast}");
    }
    let text = if is_f32 {
        format!("{:?}", value as f32)
    } else {
        format!("{value:?}")
    };
    let same_width = matches!((is_f32, declared), (true, DataType::Float4) | (false, DataType::Float8));
    if same_width {
        text
    } else {
        format!("{text}::{cast}")
    }
}

/// The element type of a declared array type, if it is one.
fn array_element_type(declared: &DataType) -> Option<&DataType> {
    match declared {
        DataType::Array(inner) => Some(inner.as_ref()),
        _ => None,
    }
}

/// TEXT form of one array ELEMENT, for the `'{…}'` literal.
///
/// Not a SQL literal — `Evaluator::parse_pg_array_text_literal` hands every
/// quoted element to `cast_value` as a `Value::String`, so what belongs here is
/// the value's text, quoted with `"` and backslash-escaped. Elements are always
/// quoted (even numbers), which is what makes a text element whose content is
/// literally `NULL` distinguishable from a real NULL member.
fn array_element_text(value: &Value, element_type: &DataType) -> Result<Option<String>> {
    let text = match value {
        Value::Null => return Ok(None),
        Value::Boolean(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Int2(v) => v.to_string(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::Float4(v) => format!("{v:?}"),
        Value::Float8(v) => format!("{v:?}"),
        Value::Numeric(n) => n.clone(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => format!("\\x{}", hex::encode(b)),
        Value::Uuid(u) => u.to_string(),
        Value::Timestamp(ts) => ts.to_rfc3339_opts(chrono::SecondsFormat::Nanos, false),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S%.9f").to_string(),
        Value::Json(j) => j.clone(),
        // No spelling exists: the array text parser splits on `,` at one level
        // only, and `cast_value` has no Interval arm.
        Value::Interval(_) | Value::Array(_) | Value::Vector(_) => {
            return Err(Error::storage(format!(
                "array element of type {} cannot be represented in a SQL dump (declared element type {element_type})",
                value.data_type()
            )))
        }
        Value::DictRef { .. } | Value::CasRef { .. } | Value::ColumnarRef => {
            return Err(Error::storage(
                "unresolved storage reference inside an array; resolve before export",
            ))
        }
    };
    Ok(Some(text))
}

/// `{"a","b",NULL}` — the PostgreSQL array text form of `items`.
fn array_text(items: &[Value], element_type: &DataType) -> Result<String> {
    let mut out = String::from("{");
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        match array_element_text(item, element_type)? {
            None => out.push_str("NULL"),
            Some(text) => {
                out.push('"');
                for ch in text.chars() {
                    if ch == '"' || ch == '\\' {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                out.push('"');
            }
        }
    }
    out.push('}');
    Ok(out)
}

/// A SQL literal for `value`, typed against the column it came from.
///
/// EXHAUSTIVE over `Value` on purpose — there is no `_` arm, so a new variant
/// breaks this build rather than silently inheriting a `{:?}` rendering, which
/// is precisely the defect HDB-005 reported.
///
/// `declared` decides the cases the text alone leaves ambiguous: `TIMESTAMP` vs
/// `TIMESTAMPTZ`, `JSON` vs `JSONB`, the element type an array casts to, and
/// whether a float needs its width spelled out.
pub(crate) fn value_literal(value: &Value, declared: &DataType) -> Result<String> {
    let literal = match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => (if *b { "TRUE" } else { "FALSE" }).to_string(),
        Value::Int2(v) => v.to_string(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::Float4(v) => float_literal(f64::from(*v), true, declared),
        Value::Float8(v) => float_literal(*v, false, declared),
        // ALWAYS quoted + cast: a bare decimal re-parses through f64.
        Value::Numeric(n) => format!("{}::numeric", quote_literal(n)),
        Value::String(s) => quote_literal(s),
        // `E'\\x…'` — sqlparser processes the escape, so the planner sees the
        // text `\x6162`, which the BYTEA cast hex-decodes.
        Value::Bytes(b) => format!("E'\\\\x{}'::bytea", hex::encode(b)),
        Value::Uuid(u) => format!("'{u}'::uuid"),
        Value::Timestamp(ts) => {
            if matches!(declared, DataType::Timestamp) {
                // A naive TIMESTAMP keeps the written wall clock; the stored
                // value's `naive_utc()` IS that wall clock.
                format!("'{}'::timestamp", ts.naive_utc().format("%Y-%m-%d %H:%M:%S%.9f"))
            } else {
                format!(
                    "'{}'::timestamptz",
                    ts.to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
                )
            }
        }
        Value::Date(d) => format!("'{}'::date", d.format("%Y-%m-%d")),
        Value::Time(t) => format!("'{}'::time", t.format("%H:%M:%S%.9f")),
        Value::Interval(micros) => format!("INTERVAL '{micros} microseconds'"),
        Value::Json(j) => {
            let cast = if matches!(declared, DataType::Json) {
                "json"
            } else {
                "jsonb"
            };
            format!("{}::{cast}", quote_literal(j))
        }
        Value::Array(items) => match array_element_type(declared) {
            Some(element_type) => format!(
                "{}::{}[]",
                quote_literal(&array_text(items, element_type)?),
                element_type
            ),
            // Not declared as an array (a dump of a legacy row, or a column
            // whose type changed): emit the text form without a cast rather
            // than inventing an element type.
            None => quote_literal(&array_text(items, &DataType::Text)?),
        },
        Value::Vector(v) => {
            // The dimension is ALWAYS spelled out. A bare `::vector` makes the
            // planner infer the dimension by counting the literal's elements,
            // which rejects an empty vector outright ("bare ::vector cast
            // requires a non-empty literal"), and the declared width is right
            // here in `declared`.
            let dimension = match declared {
                DataType::Vector(n) => *n,
                _ => v.len(),
            };
            let elements: Vec<String> = v.iter().map(|f| format!("{f:?}")).collect();
            format!("'[{}]'::vector({dimension})", elements.join(","))
        }
        Value::DictRef { .. } | Value::CasRef { .. } | Value::ColumnarRef => {
            return Err(Error::storage(
                "unresolved storage reference; resolve it before export (never emit a placeholder)",
            ))
        }
    };
    Ok(literal)
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// The stored form of a column DEFAULT / CHECK body, if it is `LogicalExpr` JSON.
///
/// `Column::default_expr` and `CheckConstraint::expression` both hold a
/// serde_json `LogicalExpr`, not SQL text.
fn stored_expr(stored: &str) -> Option<LogicalExpr> {
    serde_json::from_str::<LogicalExpr>(stored).ok()
}

/// One column definition inside `CREATE TABLE`.
///
/// `inline_primary_key` is the caller's decision, not the column's: a COMPOSITE
/// primary key marks EVERY member column `primary_key = true`, and repeating
/// `PRIMARY KEY` on each of them would declare several single-column keys.
/// `table_ddl` passes `true` only when the table has exactly one PK column.
///
/// `inline_unique` is the same kind of decision: a named `UniqueConstraint`
/// record for this one column is emitted at TABLE level (`CONSTRAINT "my_uq"
/// UNIQUE ("v")`) so the name survives the round trip, and then the inline
/// `UNIQUE` must not be written as well.
///
/// The default is rendered through [`LogicalExpr::to_dump_sql`] — the dump
/// renderer, not the catalog-readback one — so a literal containing a newline
/// is `E'…'`-escaped (the module's one-statement-per-line invariant) and an
/// expression the exporter cannot spell is an error rather than a `{:?}`
/// rendering in the file (NIT 8). A `default_expr` that is not `LogicalExpr`
/// JSON at all is passed through verbatim, as it was before: it is then already
/// SQL text written by some other path.
pub(crate) fn column_ddl(col: &Column, inline_primary_key: bool, inline_unique: bool) -> Result<String> {
    let mut out = format!("{} {}", quote_ident(&col.name), col.data_type);
    if inline_primary_key && col.primary_key {
        out.push_str(" PRIMARY KEY");
    }
    if !col.nullable && !(inline_primary_key && col.primary_key) {
        out.push_str(" NOT NULL");
    }
    if inline_unique && col.unique && !col.primary_key {
        out.push_str(" UNIQUE");
    }
    if let Some(stored) = &col.default_expr {
        let rendered = match stored_expr(stored) {
            Some(expr) => expr.to_dump_sql().map_err(|e| {
                Error::storage(format!(
                    "dump_sql: DEFAULT on column {} uses an expression the SQL exporter cannot render \
                     ({e}); drop or simplify it before exporting",
                    quote_ident(&col.name)
                ))
            })?,
            None => stored.clone(),
        };
        out.push_str(" DEFAULT ");
        out.push_str(&rendered);
    }
    Ok(out)
}

/// PostgreSQL spelling of a referential action, for `ON DELETE` / `ON UPDATE`.
fn referential_action_sql(action: ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

/// `("a", "b")`
fn quoted_column_list(columns: &[String]) -> String {
    let quoted: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
    format!("({})", quoted.join(", "))
}

/// The whole `CREATE TABLE IF NOT EXISTS …;` for one table, on ONE line.
///
/// FOREIGN KEYs are deliberately NOT included — they are emitted as trailing
/// `ALTER TABLE … ADD CONSTRAINT` statements (see [`foreign_key_ddl`]) so that a
/// dump whose tables reference each other, in any order and including cycles,
/// loads without a topological sort, and so the data can be inserted before any
/// referential check exists.
///
/// De-duplication rules, because the same declaration is recorded twice:
/// `CREATE TABLE t (id INT PRIMARY KEY)` sets `col.primary_key` AND writes a
/// `t_id_pkey` unique constraint, and a single-column `UNIQUE` — inline or
/// table-level — is propagated into `col.unique` as well as kept as a named
/// constraint record. PRIMARY KEY is expressed by the column flags and its
/// record skipped; UNIQUE is expressed by its RECORD (so the name survives) and
/// the matching column's inline `UNIQUE` suppressed.
pub(crate) fn table_ddl(table: &str, schema: &Schema, constraints: &TableConstraints) -> Result<String> {
    let pk_columns: Vec<String> = schema
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.clone())
        .collect();
    let inline_pk = pk_columns.len() == 1;

    // FIX 5: the single columns a NAMED unique constraint record already
    // covers. `CREATE TABLE t (v TEXT UNIQUE)` records `t_v_unique` and
    // `CONSTRAINT my_uq UNIQUE (v)` records `my_uq`, and BOTH propagate into
    // `col.unique` — so emitting the inline `UNIQUE` and skipping the record
    // (what this did before) threw the NAME away: `my_uq` came back as the
    // auto-generated `t_v_unique`, `information_schema.table_constraints`
    // disagreed with the source, and a migration's `DROP CONSTRAINT my_uq`
    // failed after a restore. The record wins; the inline `UNIQUE` is kept only
    // for a column that has no record at all.
    let named_unique_columns: Vec<&str> = constraints
        .unique_constraints
        .iter()
        .filter(|u| !u.is_primary_key && u.columns.len() == 1)
        .filter_map(|u| u.columns.first())
        .map(String::as_str)
        .collect();

    let mut parts: Vec<String> = Vec::with_capacity(schema.columns.len() + 4);
    for col in &schema.columns {
        let inline_unique = !named_unique_columns
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&col.name));
        parts.push(column_ddl(col, inline_pk, inline_unique)?);
    }

    if pk_columns.len() > 1 {
        parts.push(format!("PRIMARY KEY {}", quoted_column_list(&pk_columns)));
    }

    for unique in &constraints.unique_constraints {
        if unique.is_primary_key {
            // Already expressed by the column flags above.
            continue;
        }
        parts.push(format!(
            "CONSTRAINT {} UNIQUE {}",
            quote_constraint_name(&unique.name),
            quoted_column_list(&unique.columns)
        ));
    }

    for check in &constraints.check_constraints {
        // BLOCK 1: the CHECK body is rendered by the DUMP renderer, which
        // quotes identifiers and errors on anything it cannot spell. The
        // catalog-readback renderer (`to_default_sql`) writes a column
        // reference BARE, so `CHECK ("a b" > 0)` came out as `CHECK ((a b >
        // 0))` — a syntax error on the FIRST statement of the file, i.e. a
        // dump that restores nothing — and `CHECK ("createdAt" > …)` came out
        // unquoted, silently re-binding on restore to a `createdat` column that
        // does not exist. It also debug-printed `Case`/`InSet`/aggregates.
        //
        // A CHECK whose stored expression is not `LogicalExpr` JSON at all is
        // still skipped (there is nothing to render); one that IS readable but
        // cannot be spelled fails the export, naming the constraint.
        let Some(expr) = stored_expr(&check.expression) else {
            // Same policy as an unrenderable expression: never write a file
            // that silently lacks a constraint the source enforces.
            return Err(Error::storage(format!(
                "dump_sql: CHECK constraint {} on {} has a stored expression the SQL exporter cannot read; \
                 drop and recreate it before exporting",
                quote_ident(&check.name),
                quote_table_name(table)
            )));
        };
        let expression = expr.to_dump_sql().map_err(|e| {
            Error::storage(format!(
                "dump_sql: CHECK constraint {} on {} uses an expression the SQL exporter cannot render \
                 ({e}); drop or simplify it before exporting",
                quote_ident(&check.name),
                quote_table_name(table)
            ))
        })?;
        parts.push(format!(
            "CONSTRAINT {} CHECK ({})",
            quote_constraint_name(&check.name),
            expression
        ));
    }

    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {} ({});",
        quote_table_name(table),
        parts.join(", ")
    ))
}

/// `ALTER TABLE "t" ADD CONSTRAINT c FOREIGN KEY (…) REFERENCES "p" (…) …;`
///
/// Carries `fk.enforcement` (FIX 4). A `NOT ENFORCED` foreign key is the
/// spelling people reach for precisely BECAUSE the data has dangling
/// references; re-importing it as an enforced key either makes the restored
/// database reject writes the source accepted, or — worse — makes this very
/// `ADD CONSTRAINT` reject the rows the dump just loaded, so a dump of a
/// perfectly valid database could not be restored at all.
///
/// `Immediate` is the default and is written as nothing. `Deferred` and
/// `LockFree` have NO `CREATE TABLE` / `ALTER TABLE` grammar in this engine
/// (`Planner::convert_constraint_enforcement` only ever yields `Immediate` or
/// `NotEnforced`, and the `ALTER TABLE … ALTER CONSTRAINT` fast path the same),
/// so there is no spelling to emit: the mode is recorded in a trailing `-- …`
/// comment, which the script splitter drops, rather than silently vanishing.
pub(crate) fn foreign_key_ddl(table: &str, fk: &ForeignKeyConstraint) -> String {
    let mut out = format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY {} REFERENCES {} {}",
        quote_table_name(table),
        quote_constraint_name(&fk.name),
        quoted_column_list(&fk.columns),
        quote_table_name(&fk.references_table),
        quoted_column_list(&fk.references_columns)
    );
    if fk.on_delete != ReferentialAction::NoAction {
        out.push_str(" ON DELETE ");
        out.push_str(referential_action_sql(fk.on_delete));
    }
    if fk.on_update != ReferentialAction::NoAction {
        out.push_str(" ON UPDATE ");
        out.push_str(referential_action_sql(fk.on_update));
    }
    if fk.deferrable {
        out.push_str(" DEFERRABLE");
        if fk.initially_deferred {
            out.push_str(" INITIALLY DEFERRED");
        }
    }
    let unspellable = match fk.enforcement {
        ConstraintEnforcement::Immediate => None,
        ConstraintEnforcement::NotEnforced => {
            out.push_str(" NOT ENFORCED");
            None
        }
        other => Some(other),
    };
    out.push(';');
    if let Some(mode) = unspellable {
        out.push_str(&format!(
            " -- NOTE: enforcement mode {mode} has no DDL spelling and is not carried"
        ));
    }
    out
}

/// `CREATE [UNIQUE] INDEX "name" ON "t" [USING hnsw] ("col", …);`
///
/// Mirrors `EmbeddedDatabase::create_index` (the binary-restore path in
/// `embedded_db_dump.rs`) so the two dump formats declare the same indexes,
/// with the identifiers quoted.
///
/// Product quantization is NOT carried (NIT 9), and the loss is stated in a
/// `-- NOTE:` line above the statement rather than left silent. The engine does
/// have a grammar for it — `WITH (quantization = 'product')` — but
/// `IndexMetadata` records only the index TYPE (`hnsw_pq`), not the
/// `pq_subquantizers` / `pq_centroids` the codebook was trained with, and PQ
/// training refuses a table with fewer rows than `num_centroids`
/// (`train_codebook`'s `InsufficientTrainingData`). Re-emitting the option
/// would therefore restore a DIFFERENT index when it worked and turn a
/// loadable dump into an unloadable one whenever rows had since been deleted;
/// a plain HNSW index answers the same queries, more slowly.
pub(crate) fn index_ddl(table: &str, index: &IndexMetadata) -> String {
    let using = match index.index_type.as_str() {
        "hnsw" | "hnsw_pq" | "persistent_hnsw" | "persistent_hnsw_pq" => " USING hnsw",
        "hash" => " USING hash",
        "gin" => " USING gin",
        _ => "",
    };
    let unique = if index.is_unique { "UNIQUE " } else { "" };
    let note = if index.index_type.ends_with("_pq") {
        format!(
            "-- NOTE: index {} was {}; product quantization is not carried, the restored index is a plain HNSW\n",
            sanitize_comment(&index.name),
            sanitize_comment(&index.index_type)
        )
    } else {
        String::new()
    };
    format!(
        "{}CREATE {}INDEX {} ON {}{} {};",
        note,
        unique,
        quote_ident(&index.name),
        quote_table_name(table),
        using,
        quoted_column_list(&index.columns)
    )
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// `(v1, v2, …)` — one row's VALUES tuple.
fn row_values(table: &str, schema: &Schema, row: &Tuple) -> Result<String> {
    // A row stored under an older, narrower schema has no value for a column
    // added since; the catalog reads those as NULL, so the dump must too.
    let missing = Value::Null;
    let mut rendered: Vec<String> = Vec::with_capacity(schema.columns.len());
    for (idx, col) in schema.columns.iter().enumerate() {
        let value = row.values.get(idx).unwrap_or(&missing);
        let literal = value_literal(value, &col.data_type)
            .map_err(|e| Error::storage(format!("cannot export {}.{}: {}", table, col.name, e)))?;
        rendered.push(literal);
    }
    Ok(format!("({})", rendered.join(", ")))
}

/// `INSERT INTO "t" ("c1", "c2") VALUES (…);` for a single row.
///
/// The column list is ALWAYS explicit, so a restore into a table whose column
/// order later differs still lands every value in the right column. The
/// exporter batches, so this single-row form is the unit-test / one-off entry
/// point onto the same rendering.
#[allow(dead_code)]
pub(crate) fn insert_statement(table: &str, schema: &Schema, row: &Tuple) -> Result<String> {
    insert_statement_batch(table, schema, std::slice::from_ref(row))
}

/// `INSERT INTO "t" (…) VALUES (…), (…);` for up to [`INSERT_BATCH_ROWS`] rows,
/// on ONE line. Returns an empty string for an empty batch.
pub(crate) fn insert_statement_batch(table: &str, schema: &Schema, rows: &[Tuple]) -> Result<String> {
    if rows.is_empty() {
        return Ok(String::new());
    }
    let columns: Vec<String> = schema.columns.iter().map(|c| quote_ident(&c.name)).collect();
    let mut tuples: Vec<String> = Vec::with_capacity(rows.len());
    for row in rows {
        tuples.push(row_values(table, schema, row)?);
    }
    Ok(format!(
        "INSERT INTO {} ({}) VALUES {};",
        quote_table_name(table),
        columns.join(", "),
        tuples.join(", ")
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sql::{CheckConstraint, UniqueConstraint};

    fn col(name: &str, ty: DataType) -> Column {
        Column::new(name, ty)
    }

    #[test]
    fn identifiers_are_always_quoted_and_embedded_quotes_doubled() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("my table"), "\"my table\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_ident("q\"uote"), "\"q\"\"uote\"");
    }

    #[test]
    fn qualified_table_names_split_at_the_first_dot() {
        assert_eq!(quote_table_name("t"), "\"t\"");
        assert_eq!(quote_table_name("s.t"), "\"s\".\"t\"");
    }

    #[test]
    fn plain_literals_double_the_apostrophe() {
        assert_eq!(quote_literal("O'Brien"), "'O''Brien'");
        assert_eq!(quote_literal(""), "''");
        assert_eq!(quote_literal("héllo 日本"), "'héllo 日本'");
    }

    #[test]
    fn escape_literals_keep_the_statement_on_one_line() {
        assert_eq!(quote_literal("a\nb"), "E'a\\nb'");
        assert_eq!(quote_literal("a\tb"), "E'a\\tb'");
        assert_eq!(quote_literal("a\\b"), "E'a\\\\b'");
        assert_eq!(quote_literal("it's\\n"), "E'it''s\\\\n'");
        for rendered in [quote_literal("a\nb"), quote_literal("a\r\nb"), quote_literal("a\\b")] {
            assert!(!rendered.contains('\n'), "{rendered} must stay on one line");
            assert!(!rendered.contains('\r'), "{rendered} must stay on one line");
        }
    }

    /// The reported defect, at the unit level: no `{:?}` rendering survives.
    #[test]
    fn no_value_renders_as_a_rust_debug_string() {
        let cases = [
            (Value::Json("[\"x\"]".to_string()), DataType::Jsonb),
            (Value::Numeric("3.14159".to_string()), DataType::Numeric),
            (Value::Uuid(uuid::Uuid::nil()), DataType::Uuid),
            (
                Value::Array(vec![Value::String("p".to_string())]),
                DataType::Array(Box::new(DataType::Text)),
            ),
        ];
        for (value, declared) in cases {
            let rendered = value_literal(&value, &declared).unwrap();
            for marker in ["Json(", "Numeric(", "Uuid(", "Array(", "String("] {
                assert!(
                    !rendered.contains(marker),
                    "{rendered} leaked a Rust debug rendering ({marker})"
                );
            }
        }
    }

    #[test]
    fn scalar_literals_carry_the_type_the_text_alone_would_lose() {
        assert_eq!(value_literal(&Value::Null, &DataType::Text).unwrap(), "NULL");
        assert_eq!(
            value_literal(&Value::Boolean(true), &DataType::Boolean).unwrap(),
            "TRUE"
        );
        assert_eq!(value_literal(&Value::Int4(-7), &DataType::Int4).unwrap(), "-7");
        assert_eq!(
            value_literal(&Value::Numeric("3.14159".into()), &DataType::Numeric).unwrap(),
            "'3.14159'::numeric"
        );
        assert_eq!(
            value_literal(&Value::Bytes(vec![0x00, 0xab]), &DataType::Bytea).unwrap(),
            "E'\\\\x00ab'::bytea"
        );
        assert_eq!(
            value_literal(&Value::Vector(vec![1.0, 2.5]), &DataType::Vector(2)).unwrap(),
            "'[1.0,2.5]'::vector(2)"
        );
        // The declared dimension, not the literal's — a bare `::vector` cannot
        // infer one from an empty list and the planner rejects it outright.
        assert_eq!(
            value_literal(&Value::Vector(vec![]), &DataType::Vector(3)).unwrap(),
            "'[]'::vector(3)"
        );
        assert_eq!(
            value_literal(&Value::Interval(5), &DataType::Interval).unwrap(),
            "INTERVAL '5 microseconds'"
        );
    }

    #[test]
    fn floats_keep_their_shortest_round_trip_form() {
        assert_eq!(
            value_literal(&Value::Float8(1e300), &DataType::Float8).unwrap(),
            "1e300"
        );
        assert_eq!(value_literal(&Value::Float8(-0.0), &DataType::Float8).unwrap(), "-0.0");
        assert_eq!(
            value_literal(&Value::Float8(f64::NAN), &DataType::Float8).unwrap(),
            "'NaN'::float8"
        );
        assert_eq!(
            value_literal(&Value::Float8(f64::NEG_INFINITY), &DataType::Float8).unwrap(),
            "'-Infinity'::float8"
        );
        // A width the declared type does not match is spelled out.
        assert_eq!(
            value_literal(&Value::Float4(1.5), &DataType::Float8).unwrap(),
            "1.5::float4"
        );
    }

    #[test]
    fn json_follows_the_declared_type() {
        assert_eq!(
            value_literal(&Value::Json("{\"a\":1}".into()), &DataType::Json).unwrap(),
            "'{\"a\":1}'::json"
        );
        assert_eq!(
            value_literal(&Value::Json("{\"a\":1}".into()), &DataType::Jsonb).unwrap(),
            "'{\"a\":1}'::jsonb"
        );
    }

    #[test]
    fn arrays_use_the_pg_text_form_with_quoted_elements() {
        let declared = DataType::Array(Box::new(DataType::Text));
        let value = Value::Array(vec![
            Value::String("a,b".to_string()),
            Value::Null,
            Value::String("NULL".to_string()),
            Value::String(String::new()),
        ]);
        assert_eq!(
            value_literal(&value, &declared).unwrap(),
            "'{\"a,b\",NULL,\"NULL\",\"\"}'::TEXT[]"
        );
        assert_eq!(
            value_literal(&Value::Array(vec![]), &DataType::Array(Box::new(DataType::Int4))).unwrap(),
            "'{}'::INT4[]"
        );
    }

    #[test]
    fn storage_references_are_an_error_not_a_placeholder() {
        for value in [
            Value::DictRef { dict_id: 7 },
            Value::CasRef { hash: [0u8; 32] },
            Value::ColumnarRef,
        ] {
            let err = value_literal(&value, &DataType::Text).unwrap_err();
            assert!(err.to_string().contains("storage reference"), "unexpected error: {err}");
        }
    }

    #[test]
    fn single_column_primary_key_is_inline_and_composite_is_table_level() {
        let mut a = col("a", DataType::Int4);
        a.primary_key = true;
        a.nullable = false;
        let mut b = col("b", DataType::Int4);
        b.primary_key = true;
        b.nullable = false;
        let single = Schema::new(vec![a.clone(), col("v", DataType::Text)]);
        let ddl = table_ddl("t", &single, &TableConstraints::new()).unwrap();
        assert!(ddl.contains("\"a\" INT4 PRIMARY KEY"), "{ddl}");
        assert!(!ddl.contains("PRIMARY KEY (\"a\")"), "{ddl}");

        let composite = Schema::new(vec![a, b]);
        let ddl = table_ddl("t", &composite, &TableConstraints::new()).unwrap();
        assert!(ddl.contains("PRIMARY KEY (\"a\", \"b\")"), "{ddl}");
        assert_eq!(ddl.matches("PRIMARY KEY").count(), 1, "{ddl}");
    }

    /// FIX 5: ONE `UNIQUE`, and it is the NAMED table-level one — the inline
    /// spelling would throw the constraint's name away on restore.
    #[test]
    fn a_named_single_column_unique_keeps_its_name_and_is_not_emitted_twice() {
        let mut v = col("v", DataType::Text);
        v.unique = true;
        let schema = Schema::new(vec![v]);
        let mut constraints = TableConstraints::new();
        constraints.add_unique(UniqueConstraint::new(
            "my_uq".into(),
            "t".into(),
            vec!["v".into()],
            false,
        ));
        let ddl = table_ddl("t", &schema, &constraints).unwrap();
        assert_eq!(ddl.matches("UNIQUE").count(), 1, "{ddl}");
        assert!(ddl.contains("CONSTRAINT my_uq UNIQUE (\"v\")"), "{ddl}");
        assert!(!ddl.contains("\"v\" TEXT UNIQUE"), "{ddl}");
    }

    /// …and a column flagged unique with NO constraint record keeps the inline
    /// spelling, which is the only one left that can carry it.
    #[test]
    fn a_unique_column_without_a_record_stays_inline() {
        let mut v = col("v", DataType::Text);
        v.unique = true;
        let schema = Schema::new(vec![v]);
        let ddl = table_ddl("t", &schema, &TableConstraints::new()).unwrap();
        assert!(ddl.contains("\"v\" TEXT UNIQUE"), "{ddl}");
    }

    #[test]
    fn composite_unique_and_check_reach_the_ddl() {
        let schema = Schema::new(vec![col("a", DataType::Int4), col("b", DataType::Int4)]);
        let mut constraints = TableConstraints::new();
        constraints.add_unique(UniqueConstraint::new(
            "t_ab_unique".into(),
            "t".into(),
            vec!["a".into(), "b".into()],
            false,
        ));
        constraints.add_check(CheckConstraint::new(
            "t_a_check".into(),
            "t".into(),
            serde_json::to_string(&crate::sql::LogicalExpr::BinaryExpr {
                left: Box::new(crate::sql::LogicalExpr::Column {
                    table: None,
                    name: "a".into(),
                }),
                op: crate::sql::BinaryOperator::Gt,
                right: Box::new(crate::sql::LogicalExpr::Literal(Value::Int4(0))),
            })
            .unwrap(),
        ));
        let ddl = table_ddl("t", &schema, &constraints).unwrap();
        assert!(ddl.contains("CONSTRAINT t_ab_unique UNIQUE (\"a\", \"b\")"), "{ddl}");
        // BLOCK 1: the CHECK body QUOTES its column reference, so the same
        // rendering works for `a`, for `"a b"` and for `"createdAt"`.
        assert!(ddl.contains("CONSTRAINT t_a_check CHECK ((\"a\" > 0))"), "{ddl}");
        assert!(!ddl.contains('\n'), "DDL must be one line: {ddl}");
    }

    fn check_on(name: &str, expr: crate::sql::LogicalExpr) -> TableConstraints {
        let mut constraints = TableConstraints::new();
        constraints.add_check(CheckConstraint::new(
            name.into(),
            "t".into(),
            serde_json::to_string(&expr).unwrap(),
        ));
        constraints
    }

    /// The reported headline defect, at the unit level: a CHECK on a column
    /// whose name needs quoting used to be written `CHECK ((a b > 0))` — the
    /// FIRST statement of the file, so the whole dump restored nothing.
    #[test]
    fn a_check_on_a_quoted_column_is_quoted_in_the_ddl() {
        let schema = Schema::new(vec![
            col("a b", DataType::Int4),
            col("createdAt", DataType::Timestamptz),
        ]);
        let constraints = check_on(
            "t_chk",
            crate::sql::LogicalExpr::BinaryExpr {
                left: Box::new(crate::sql::LogicalExpr::Column {
                    table: None,
                    name: "a b".into(),
                }),
                op: crate::sql::BinaryOperator::Gt,
                right: Box::new(crate::sql::LogicalExpr::Literal(Value::Int4(0))),
            },
        );
        let ddl = table_ddl("t", &schema, &constraints).unwrap();
        assert!(ddl.contains("CHECK ((\"a b\" > 0))"), "{ddl}");
        assert!(!ddl.contains("(a b >"), "{ddl}");
    }

    /// The implicit enum CHECK shape (`planner.rs`'s `InList` over the enum's
    /// labels), which is how every `"userRole" user_role` column is enforced.
    #[test]
    fn an_enum_check_in_list_round_trips_quoted() {
        let schema = Schema::new(vec![col("userRole", DataType::Text)]);
        let constraints = check_on(
            "t_userrole_check",
            crate::sql::LogicalExpr::InList {
                expr: Box::new(crate::sql::LogicalExpr::Column {
                    table: None,
                    name: "userRole".into(),
                }),
                list: vec![
                    crate::sql::LogicalExpr::Literal(Value::String("admin".into())),
                    crate::sql::LogicalExpr::Literal(Value::String("user".into())),
                ],
                negated: false,
            },
        );
        let ddl = table_ddl("t", &schema, &constraints).unwrap();
        assert!(ddl.contains("CHECK ((\"userRole\" IN ('admin', 'user')))"), "{ddl}");
    }

    /// A shape the dump renderer cannot spell fails the EXPORT, naming the
    /// constraint — rather than writing `CHECK (Case { operand: None, … })`
    /// into a file that will not load.
    #[test]
    fn an_unrenderable_check_fails_the_export_by_name() {
        let schema = Schema::new(vec![col("a", DataType::Int4)]);
        let constraints = check_on(
            "t_case_chk",
            crate::sql::LogicalExpr::Case {
                expr: None,
                when_then: vec![(
                    crate::sql::LogicalExpr::Literal(Value::Boolean(true)),
                    crate::sql::LogicalExpr::Literal(Value::Boolean(true)),
                )],
                else_result: None,
            },
        );
        let err = table_ddl("t", &schema, &constraints).unwrap_err().to_string();
        assert!(err.contains("t_case_chk"), "{err}");
        assert!(err.contains("CHECK constraint"), "{err}");
        assert!(err.contains("CASE"), "{err}");
    }

    #[test]
    fn foreign_keys_are_trailing_alter_statements() {
        let fk = ForeignKeyConstraint::new(
            "fk_child_pid__parent".into(),
            "child".into(),
            vec!["pid".into()],
            "parent".into(),
            vec!["id".into()],
        )
        .on_delete(ReferentialAction::Cascade)
        .deferrable(true);
        assert_eq!(
            foreign_key_ddl("child", &fk),
            "ALTER TABLE \"child\" ADD CONSTRAINT fk_child_pid__parent FOREIGN KEY (\"pid\") \
             REFERENCES \"parent\" (\"id\") ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED;"
        );
    }

    #[test]
    fn insert_names_its_columns_and_stays_on_one_line() {
        let schema = Schema::new(vec![col("id", DataType::Int4), col("name", DataType::Text)]);
        let stmt = insert_statement(
            "my table",
            &schema,
            &Tuple::new(vec![Value::Int4(1), Value::String("Ali\nce".into())]),
        )
        .unwrap();
        assert_eq!(
            stmt,
            "INSERT INTO \"my table\" (\"id\", \"name\") VALUES (1, E'Ali\\nce');"
        );
        assert!(!stmt.contains('\n'));
    }

    #[test]
    fn a_batch_is_one_statement() {
        let schema = Schema::new(vec![col("id", DataType::Int4)]);
        let rows = vec![Tuple::new(vec![Value::Int4(1)]), Tuple::new(vec![Value::Int4(2)])];
        assert_eq!(
            insert_statement_batch("t", &schema, &rows).unwrap(),
            "INSERT INTO \"t\" (\"id\") VALUES (1), (2);"
        );
        assert_eq!(insert_statement_batch("t", &schema, &[]).unwrap(), "");
    }

    #[test]
    fn index_ddl_quotes_and_keeps_the_access_method() {
        let index = IndexMetadata {
            name: "idx_v".into(),
            index_type: "hnsw".into(),
            columns: vec!["embedding".into()],
            is_unique: false,
        };
        assert_eq!(
            index_ddl("t", &index),
            "CREATE INDEX \"idx_v\" ON \"t\" USING hnsw (\"embedding\");"
        );
    }

    #[test]
    fn comments_cannot_break_the_script_splitter() {
        let sanitized = sanitize_comment("it's; $$ \"x\"\nnext");
        for ch in ['\'', ';', '$', '"', '\n'] {
            assert!(!sanitized.contains(ch), "{sanitized}");
        }
    }

    #[test]
    fn column_ddl_renders_the_stored_default_as_sql() {
        let mut c = col("created", DataType::Timestamptz);
        c.default_expr = Some(
            serde_json::to_string(&crate::sql::LogicalExpr::ScalarFunction {
                fun: "now".into(),
                args: vec![],
            })
            .unwrap(),
        );
        assert_eq!(
            column_ddl(&c, false, true).unwrap(),
            "\"created\" TIMESTAMPTZ DEFAULT now()"
        );
    }

    /// NIT 8: the DEFAULT goes through the dump renderer, so a literal holding
    /// a newline is `E'…'`-escaped and the statement stays on ONE line —
    /// `render_default_literal` (the catalog readback path) emits the raw
    /// newline and, for anything it has no arm for, a `{:?}` rendering.
    #[test]
    fn a_default_literal_with_a_newline_stays_on_one_line() {
        let mut c = col("note", DataType::Text);
        c.default_expr =
            Some(serde_json::to_string(&crate::sql::LogicalExpr::Literal(Value::String("a\nb".into()))).unwrap());
        let ddl = column_ddl(&c, false, true).unwrap();
        assert_eq!(ddl, "\"note\" TEXT DEFAULT E'a\\nb'");
        assert!(!ddl.contains('\n'), "{ddl}");
    }

    /// FIX 4: a `NOT ENFORCED` foreign key says so on the way out.
    #[test]
    fn foreign_key_enforcement_is_carried() {
        let fk = ForeignKeyConstraint::new(
            "fk_c".into(),
            "child".into(),
            vec!["pid".into()],
            "parent".into(),
            vec!["id".into()],
        );
        assert!(!foreign_key_ddl("child", &fk).contains("ENFORCED"));

        let not_enforced = fk.clone().with_enforcement(ConstraintEnforcement::NotEnforced);
        assert!(
            foreign_key_ddl("child", &not_enforced).ends_with("(\"id\") NOT ENFORCED;"),
            "{}",
            foreign_key_ddl("child", &not_enforced)
        );

        // No DDL spelling exists for these two; the loss is stated, not hidden.
        let lock_free = fk.with_enforcement(ConstraintEnforcement::LockFree);
        let ddl = foreign_key_ddl("child", &lock_free);
        assert!(ddl.contains("-- NOTE: enforcement mode LOCK-FREE"), "{ddl}");
        assert!(!ddl.contains("LOCK-FREE;"), "{ddl}");
    }

    /// NIT 9: a quantized vector index restores as a plain HNSW, and the dump
    /// says so instead of dropping the fact.
    #[test]
    fn a_quantized_index_records_what_it_loses() {
        let index = IndexMetadata {
            name: "idx_pq".into(),
            index_type: "hnsw_pq".into(),
            columns: vec!["embedding".into()],
            is_unique: false,
        };
        let ddl = index_ddl("t", &index);
        assert!(ddl.starts_with("-- NOTE: index idx_pq was hnsw_pq;"), "{ddl}");
        assert!(
            ddl.ends_with("CREATE INDEX \"idx_pq\" ON \"t\" USING hnsw (\"embedding\");"),
            "{ddl}"
        );
    }

    /// FIX 3: nine fractional digits, because that is what the engine stores
    /// and what `%.f` reads back.
    #[test]
    fn temporal_literals_keep_nanoseconds() {
        let time = chrono::NaiveTime::from_hms_nano_opt(1, 0, 0, 123_456_789).unwrap();
        assert_eq!(
            value_literal(&Value::Time(time), &DataType::Time).unwrap(),
            "'01:00:00.123456789'::time"
        );
        let ts = chrono::DateTime::from_timestamp(1_755_306_000, 123_456_789).unwrap();
        assert_eq!(
            value_literal(&Value::Timestamp(ts), &DataType::Timestamp).unwrap(),
            "'2025-08-16 01:00:00.123456789'::timestamp"
        );
        assert!(value_literal(&Value::Timestamp(ts), &DataType::Timestamptz)
            .unwrap()
            .contains(".123456789"));
        // …and inside an array, which renders through `array_element_text`.
        assert_eq!(
            value_literal(
                &Value::Array(vec![Value::Time(time)]),
                &DataType::Array(Box::new(DataType::Time))
            )
            .unwrap(),
            "'{\"01:00:00.123456789\"}'::TIME[]"
        );
    }
}
