//! Prisma P0 spec 02 — `RETURNING` output columns must be named the way
//! `SELECT` output columns are named.
//!
//! ## The defect
//!
//! Through v4.30.0 the planner named an unaliased `RETURNING` item with
//! `format!("{expr}")` — sqlparser's `Display`, which re-emits the identifier
//! **quote characters**. Prisma 7.10 fully qualifies every `RETURNING` item,
//! so a `prisma.account.create()` came back over the wire as
//!
//! ```text
//!   field name: "public"."Account"."id"      (PostgreSQL: id)
//! ```
//!
//! and the portal needed a client-side rename shim (`nano-adapter.ts`) to map
//! its own rows. The SELECT list has never had the problem —
//! `SELECT "public"."Account"."id" FROM "public"."Account"` is named `id`
//! already — because the projection list names items through
//! `Planner::extract_expr_alias`. Only `Planner::convert_returning` kept the
//! raw text.
//!
//! A name-only fix would not have been enough, and the tests below say why: a
//! qualified reference lowered to `ReturningItem::Expression` also gets the
//! wrong TYPE (`returning_schema` hard-codes every `Expression` item to
//! `DataType::Text`/OID 25, so `RETURNING "public"."Account"."id"` described an
//! INT column as text while `RETURNING id` described it as int4) and the wrong
//! VALUE (`project_returning_columns` evaluates an `Expression` through
//! `Evaluator::evaluate`, which matches the qualifier against the catalog's
//! stamped `source_table_name` byte-exactly — so a case-folded qualifier
//! `Account.id` or an alias qualifier `x."id"` missed and produced NULL). A
//! qualified reference is therefore lowered to `ReturningItem::Column` on its
//! bare part: right name, right type, right value. The OID half is pinned on
//! the wire in `src/protocol/postgres/wire_tests.rs`, the only surface that can
//! observe it.
//!
//! ## What these tests pin
//!
//! * Every qualification spelling of a column reference (`"schema"."tbl"."col"`,
//!   `"tbl"."col"`, `tbl.col`, `alias."col"`, `"col"`, `col`) names the output
//!   column by its bare column part, for INSERT, UPDATE and DELETE alike.
//! * …and RESOLVES to the target table's value, never to a silent NULL.
//! * `RETURNING <expr>` is named EXACTLY what `SELECT <expr>` is named — the
//!   parity table below is the anti-drift assertion, since the fix is "call the
//!   SELECT naming function" rather than "write a second naming function".
//! * `expr AS alias`, `*` and the projected VALUES are untouched by the rename.
//!
//! ## Families
//!
//! Naming is decided at PLAN time (`convert_returning`), upstream of both DML
//! executor families, and both read the same `ReturningItem`s. The names are
//! observable through `query_params_with_columns` — the params family
//! (`execute_plan_with_params_inner`), which is what the PG extended protocol,
//! the PyO3 binding, and (via `execute_returning`) the PG simple-query path all
//! land on. It is exercised here both with a literal statement (no `$n`) and
//! with bound `$n` parameters. The text family
//! (`execute()` -> `execute_in_transaction_inner`) computes a `RETURNING`
//! projection and DISCARDS it — `execute()` returns only a count — so it is
//! covered here for acceptance + row effect, and end-to-end for names by
//! `src/protocol/postgres/wire_tests.rs` (simple-query `handle_single_query`,
//! which derives its RowDescription from the same plan items).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

/// Prisma-shaped table: a quoted, mixed-case relation with a quoted camelCase
/// column, so the fix cannot be faked by lowercasing everything.
const DDL: &str = r#"CREATE TABLE "Account" (id INT PRIMARY KEY, email TEXT, "createdAt" TEXT)"#;

fn empty_db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute(DDL).unwrap();
    db
}

/// One row: `(1, 'a@example.com', '2026-09-06')`.
fn seeded_db() -> EmbeddedDatabase {
    let db = empty_db();
    db.execute(r#"INSERT INTO "public"."Account" ("id","email","createdAt") VALUES (1,'a@example.com','2026-09-06')"#)
        .unwrap();
    db
}

/// Run `sql` through the params executor family and return
/// `(rows, output column names)`.
///
/// `query_params_with_columns` is the one embedded surface that exposes DML
/// `RETURNING` column NAMES (it derives them from the plan via
/// `EmbeddedDatabase::returning_schema`, exactly like the two PG-wire
/// RowDescription sites do). An empty `params` slice is the literal-statement
/// shape; a non-empty one is the extended-protocol shape.
fn run(db: &EmbeddedDatabase, sql: &str, params: &[Value]) -> (Vec<Tuple>, Vec<String>) {
    db.query_params_with_columns(sql, params)
        .unwrap_or_else(|e| panic!("`{sql}` must execute: {e}"))
}

fn names(db: &EmbeddedDatabase, sql: &str, params: &[Value]) -> Vec<String> {
    run(db, sql, params).1
}

fn int_at(row: &Tuple, idx: usize) -> i64 {
    match row.values.get(idx) {
        Some(Value::Int2(v)) => i64::from(*v),
        Some(Value::Int4(v)) => i64::from(*v),
        Some(Value::Int8(v)) => *v,
        other => panic!("expected an integer at column {idx}, got {other:?}"),
    }
}

fn text_at(row: &Tuple, idx: usize) -> String {
    match row.values.get(idx) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text at column {idx}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The reported symptom, one statement per DML verb.
// ---------------------------------------------------------------------------

/// THE Prisma `create()` shape. FAILS on the unfixed tree with
/// `["\"public\".\"Account\".\"id\"", "\"public\".\"Account\".\"email\"",
///   "\"public\".\"Account\".\"createdAt\""]`.
#[test]
fn insert_returning_is_named_like_postgres() {
    let db = empty_db();
    let (rows, cols) = run(
        &db,
        r#"INSERT INTO "public"."Account" ("id","email","createdAt") VALUES (1,'a@example.com','2026-09-06')
           RETURNING "public"."Account"."id", "public"."Account"."email", "public"."Account"."createdAt""#,
        &[],
    );
    assert_eq!(
        cols,
        vec!["id", "email", "createdAt"],
        "a fully qualified RETURNING list must be named by the bare column parts, \
         quotes stripped and quoted case preserved"
    );
    // The rename must not disturb the projected row.
    assert_eq!(rows.len(), 1, "INSERT … RETURNING must return the inserted row");
    assert_eq!(int_at(&rows[0], 0), 1);
    assert_eq!(text_at(&rows[0], 1), "a@example.com");
    assert_eq!(text_at(&rows[0], 2), "2026-09-06");
}

/// The Prisma `update()` shape, bound through the extended-protocol
/// (`$n` parameter) call shape. FAILS on the unfixed tree with the quoted
/// three-part names.
#[test]
fn update_returning_is_named_like_postgres_with_bound_params() {
    let db = seeded_db();
    let (rows, cols) = run(
        &db,
        r#"UPDATE "public"."Account" SET "email" = $1 WHERE "public"."Account"."id" = $2
           RETURNING "public"."Account"."id", "public"."Account"."email""#,
        &[Value::String("b@example.com".to_string()), Value::Int4(1)],
    );
    assert_eq!(cols, vec!["id", "email"]);
    assert_eq!(rows.len(), 1, "UPDATE … RETURNING must return the updated row");
    assert_eq!(int_at(&rows[0], 0), 1);
    assert_eq!(
        text_at(&rows[0], 1),
        "b@example.com",
        "RETURNING must project the POST-update value"
    );
}

/// Two-part qualification (`"Account"."id"`, and the unquoted `Account.id`
/// spelling drizzle/knex emit). FAILS on the unfixed tree with
/// `"\"Account\".\"id\""` / `"Account.id"`.
#[test]
fn delete_returning_two_part_qualification_is_named_like_postgres() {
    let db = seeded_db();
    let (rows, cols) = run(
        &db,
        r#"DELETE FROM "public"."Account" WHERE "public"."Account"."id" = $1
           RETURNING "Account"."id", "Account"."email""#,
        &[Value::Int4(1)],
    );
    assert_eq!(cols, vec!["id", "email"]);
    assert_eq!(rows.len(), 1, "DELETE … RETURNING must return the deleted row");
    assert_eq!(int_at(&rows[0], 0), 1);

    // The unquoted spelling folds to the same bare NAME — and must project the
    // same VALUE. This is the fail-open half of the defect: an unquoted
    // qualifier is lower-cased by `Planner::normalize_ident` (`account`) while
    // the catalog stamps the column's `source_table_name` as written
    // (`Account`), so an `Expression` lowering resolved NOTHING and
    // `project_returning_columns` mapped the `Err` to `Value::Null`. Naming
    // that column `id` without also fixing the lowering would have handed an
    // ORM a NULL primary key under the exact name it binds. Lowering a
    // qualified reference to `ReturningItem::Column` resolves it by bare name,
    // where the qualifier cannot mis-fold.
    let db = seeded_db();
    let (rows, cols) = run(
        &db,
        r#"DELETE FROM "public"."Account" WHERE "public"."Account"."id" = 1 RETURNING Account.id"#,
        &[],
    );
    assert_eq!(cols, vec!["id"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        int_at(&rows[0], 0),
        1,
        "an unquoted qualifier must still project the target table's value, not NULL"
    );
}

/// An ALIAS qualifier (`DELETE FROM "Account" AS x … RETURNING x."id"`) is the
/// same class as the case-folded qualifier above and the one Prisma's raw-query
/// escape hatch and knex/drizzle emit: the catalog stamps the column's
/// `source_table_name` as the REAL table (`Account`), so a qualified
/// `LogicalExpr::Column` on `x` can never match and the row silently carried
/// NULL. Nothing covered it before.
///
/// FAILS on the unfixed tree: the name comes back as `x."id"` / `x."email"`.
/// It ALSO fails on a name-only fix, because the values are NULL.
#[test]
fn alias_qualified_returning_resolves_name_and_value() {
    let db = seeded_db();
    let (rows, cols) = run(&db, r#"DELETE FROM "Account" AS x RETURNING x."id", x."email""#, &[]);
    assert_eq!(cols, vec!["id", "email"]);
    assert_eq!(rows.len(), 1, "DELETE … RETURNING must return the deleted row");
    assert_eq!(
        int_at(&rows[0], 0),
        1,
        "an alias-qualified RETURNING reference must project the row, not NULL"
    );
    assert_eq!(text_at(&rows[0], 1), "a@example.com");

    // Same class through UPDATE, whose alias slot is a different parser path.
    let db = seeded_db();
    let (rows, cols) = run(
        &db,
        r#"UPDATE "Account" AS t SET "email" = 'aliased@example.com' RETURNING t."id", t."email""#,
        &[],
    );
    assert_eq!(cols, vec!["id", "email"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(int_at(&rows[0], 0), 1);
    assert_eq!(text_at(&rows[0], 1), "aliased@example.com");
}

// ---------------------------------------------------------------------------
// RETURNING naming == SELECT naming (the anti-drift contract).
// ---------------------------------------------------------------------------

/// Name that a SELECT list gives `expr` over the fixture table.
fn select_name(expr: &str) -> String {
    let db = seeded_db();
    let sql = format!(r#"SELECT {expr} FROM "public"."Account""#);
    let cols = names(&db, &sql, &[]);
    assert_eq!(
        cols.len(),
        1,
        "`{sql}` must produce exactly one output column: {cols:?}"
    );
    cols.into_iter().next().unwrap()
}

/// Name that a RETURNING list gives the same `expr`, over the same table.
/// UPDATE is used because it needs neither a fresh primary key nor a WHERE
/// clause, and it always matches the single seeded row.
fn returning_name(expr: &str) -> String {
    let db = seeded_db();
    let sql = format!(r#"UPDATE "public"."Account" SET "email" = 'z@example.com' RETURNING {expr}"#);
    let cols = names(&db, &sql, &[]);
    assert_eq!(
        cols.len(),
        1,
        "`{sql}` must produce exactly one output column: {cols:?}"
    );
    cols.into_iter().next().unwrap()
}

/// The contract: for every expression shape, `RETURNING <e>` and `SELECT <e>`
/// agree on the output column name, and that name is PostgreSQL's.
///
/// FAILS on the unfixed tree at rows 1, 2, 5, 7 and 8, where `RETURNING`
/// reported the raw expression text (`"public"."Account"."id"`,
/// `upper("email")`, `"id" + 1`, `count(*)`) while `SELECT` already reported
/// `id`, `upper(email)`, `id + 1`, `count`; and at row 6, where the two AGREED
/// on `lower(email)` but PostgreSQL says `lower`. Rows 3, 4 and 9 already
/// passed and are kept as controls.
///
/// Rows 5 and 6 additionally pin the SELECT side onto PostgreSQL's rule: an
/// unaliased function column is named after the FUNCTION ALONE, so
/// `upper("email")` is `upper`, not `upper(email)`. Through v4.30.0 that held
/// only when the argument was a wildcard (`count(*)` → `count`) or qualified
/// (`sum(t.x)` → `sum`); a bare-identifier argument kept the `func(arg)`
/// spelling. Both sides move together because both call
/// `Planner::extract_expr_alias`.
#[test]
fn returning_names_match_select_names() {
    // (expression, PostgreSQL's name for it)
    let cases: &[(&str, &str)] = &[
        (r#""public"."Account"."id""#, "id"),
        (r#""Account"."id""#, "id"),
        (r#""id""#, "id"),
        ("id", "id"),
        (r#"upper("email")"#, "upper"),
        ("lower(email)", "lower"),
        (r#""id" + 1"#, "id + 1"),
        ("count(*)", "count"),
        ("1", "1"),
    ];

    for &(expr, expected) in cases {
        let sel = select_name(expr);
        let ret = returning_name(expr);
        assert_eq!(
            ret, sel,
            "RETURNING {expr} was named `{ret}` but SELECT {expr} is named `{sel}` — \
             the two naming paths have drifted apart again"
        );
        assert_eq!(
            ret, expected,
            "RETURNING {expr} must be named `{expected}` (PostgreSQL), got `{ret}`"
        );
    }
}

/// `"MixedCase"` inside quotes keeps its case; an unquoted qualifier does not
/// leak into the name. Pinned separately from the table above because it is the
/// exact shape Prisma emits for `createdAt` / `updatedAt`.
#[test]
fn quoted_camel_case_column_keeps_its_case() {
    let db = seeded_db();
    let cols = names(
        &db,
        r#"UPDATE "public"."Account" SET "email" = 'c@example.com' RETURNING "public"."Account"."createdAt""#,
        &[],
    );
    assert_eq!(cols, vec!["createdAt"], "quoted case must survive verbatim");
    assert_eq!(
        select_name(r#""public"."Account"."createdAt""#),
        "createdAt",
        "…and must match the SELECT list, which already did this"
    );
}

// ---------------------------------------------------------------------------
// Shapes that must NOT change.
// ---------------------------------------------------------------------------

/// An explicit alias always wins, including over a qualified reference, and it
/// is preserved verbatim (quoted case included).
#[test]
fn explicit_alias_still_wins() {
    let db = seeded_db();
    let cols = names(
        &db,
        r#"UPDATE "public"."Account" SET "email" = 'd@example.com'
           RETURNING "public"."Account"."id" AS "accountId", "email" AS e"#,
        &[],
    );
    assert_eq!(cols, vec!["accountId", "e"]);
}

/// `RETURNING *` (and `t.*`) still expand to the table's own column names, in
/// declaration order — the `delete_returning_tests.rs` contract.
#[test]
fn wildcard_returning_still_names_every_table_column() {
    let db = empty_db();
    let (rows, cols) = run(
        &db,
        r#"INSERT INTO "public"."Account" ("id","email","createdAt") VALUES (7,'w@example.com','2026-09-06')
           RETURNING *"#,
        &[],
    );
    assert_eq!(cols, vec!["id", "email", "createdAt"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values.len(), 3, "RETURNING * must project the whole row");

    let db = seeded_db();
    let cols = names(
        &db,
        r#"UPDATE "public"."Account" SET "email" = 'q@example.com' RETURNING "Account".*"#,
        &[],
    );
    assert_eq!(cols, vec!["id", "email", "createdAt"], "t.* must behave like *");
}

/// Mixed list: a qualified reference, an explicit alias and a function call in
/// one RETURNING clause keep their positions, their arity and their (now
/// PostgreSQL-shaped) names — and every value still lands in its own slot.
#[test]
fn mixed_returning_list_keeps_order_and_arity() {
    let db = empty_db();
    let (rows, cols) = run(
        &db,
        r#"INSERT INTO "public"."Account" ("id","email","createdAt") VALUES (9,'m@example.com','2026-09-06')
           RETURNING "public"."Account"."id", "email" AS "Mail", upper("email")"#,
        &[],
    );
    assert_eq!(cols, vec!["id", "Mail", "upper"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(int_at(&rows[0], 0), 9);
    assert_eq!(text_at(&rows[0], 1), "m@example.com");
    assert_eq!(text_at(&rows[0], 2), "M@EXAMPLE.COM");
}

// ---------------------------------------------------------------------------
// Text executor family (`execute()` -> `execute_in_transaction_inner`).
// ---------------------------------------------------------------------------

/// `db.execute()` is the text family: `try_fast_insert` bails on `RETURNING`,
/// so the statement lands in `execute_in_transaction_inner`, which projects the
/// RETURNING list and then DISCARDS it (`execute()` yields only a count). The
/// names are therefore unobservable from this family by construction — what is
/// checkable here is that the qualified list is accepted and has the right row
/// effect. The second half re-runs the same statement shape through
/// `execute_returning` (params family) to pin the projected VALUES, and the
/// names on the text-family WIRE route (simple query, whose RowDescription
/// comes from `derive_returning_schema`) are covered end-to-end in
/// `src/protocol/postgres/wire_tests.rs`.
#[test]
fn text_family_accepts_the_qualified_returning_list() {
    let db = empty_db();
    let affected = db
        .execute(
            r#"INSERT INTO "public"."Account" ("id","email","createdAt") VALUES (2,'t@example.com','2026-09-06')
               RETURNING "public"."Account"."id", "public"."Account"."email""#,
        )
        .expect("text-family INSERT … RETURNING must be accepted");
    assert_eq!(affected, 1);

    let (count, rows) = db
        .execute_returning(
            r#"UPDATE "public"."Account" SET "email" = 'u@example.com' WHERE "public"."Account"."id" = 2
               RETURNING "public"."Account"."id", "public"."Account"."email""#,
        )
        .expect("UPDATE … RETURNING must be accepted");
    assert_eq!(count, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        int_at(&rows[0], 0),
        2,
        "a qualified RETURNING reference must project the target table's value, not NULL"
    );
    assert_eq!(text_at(&rows[0], 1), "u@example.com");
}
