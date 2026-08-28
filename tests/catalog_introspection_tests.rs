//! HC3 — catalog introspection served by ONE implementation.
//!
//! Before this change the catalog surface had three partly-divergent
//! implementations: a wire-only substring router in
//! `src/protocol/postgres/catalog.rs`, the live planner-backed
//! `SystemViewRegistry` in `src/sql/phase3/system_views.rs`, and a dead legacy
//! registry. Views, CHECK clauses, `pg_indexes` and `pg_matviews` were empty,
//! wire-only, or an outright ERROR depending on which route you arrived by.
//!
//! Every test here drives the EMBEDDED route — which is what the REPL, the
//! Python binding and `EmbeddedDatabase` users get — through BOTH DML executor
//! families:
//!   * text family   → `db.query_with_columns`        (execute_in_transaction_inner)
//!   * params family → `db.query_params_with_columns` (execute_plan_with_params_inner)
//! The PG-wire route is covered separately in `src/protocol/postgres/wire_tests.rs`;
//! the repo's documented gotcha is that embedded tests do NOT exercise
//! `protocol/postgres/catalog.rs` at all, so neither file substitutes for the other.

use heliosdb_nano::{EmbeddedDatabase, Value};

/// Render a `Value` as comparable text.
fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Run `sql` through BOTH executor families and assert they agree exactly
/// (column names, row count, every rendered cell). Returns
/// `(rows_as_text, column_names)` from the text family.
///
/// A feature that lands in only one family is this repo's signature defect, so
/// the matrix is enforced on every single catalog assertion below rather than
/// in one token "parity" test.
fn both_families(db: &EmbeddedDatabase, sql: &str) -> (Vec<Vec<String>>, Vec<String>) {
    let (text_rows, text_cols) = db
        .query_with_columns(sql)
        .unwrap_or_else(|e| panic!("text family failed on `{sql}`: {e}"));
    let (param_rows, param_cols) = db
        .query_params_with_columns(sql, &[])
        .unwrap_or_else(|e| panic!("params family failed on `{sql}`: {e}"));

    assert_eq!(
        text_cols, param_cols,
        "column list differs between executor families for `{sql}`"
    );
    let render = |rows: &[heliosdb_nano::Tuple]| -> Vec<Vec<String>> {
        rows.iter().map(|r| r.values.iter().map(s).collect()).collect()
    };
    let text = render(&text_rows);
    let params = render(&param_rows);
    assert_eq!(
        text, params,
        "rows differ between executor families for `{sql}`: text={text:?} params={params:?}"
    );
    (text, text_cols)
}

/// Index of a column by name, panicking with the full list on a miss.
fn col(cols: &[String], name: &str) -> usize {
    cols.iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("column `{name}` missing; got {cols:?}"))
}

fn db_with_a_view() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE ci_child (id INT PRIMARY KEY, qty INT)")
        .expect("create table");
    db.execute("CREATE VIEW ci_v AS SELECT id, qty FROM ci_child")
        .expect("create view");
    db
}

// ---------------------------------------------------------------------------
// P3 — views are visible on every catalog surface.
// ---------------------------------------------------------------------------

/// `pg_views` returns the REAL stored body. The deleted wire stub returned zero
/// rows behind a comment claiming Nano does not persist view definitions; it
/// always has (`ViewCatalog`, src/storage/view_catalog.rs).
#[test]
fn pg_views_exposes_the_stored_view_definition() {
    let db = db_with_a_view();
    let (rows, cols) = both_families(&db, "SELECT * FROM pg_views");
    assert_eq!(rows.len(), 1, "exactly one view was created; got {rows:?}");

    let row = &rows[0];
    assert_eq!(row[col(&cols, "schemaname")], "public");
    assert_eq!(row[col(&cols, "viewname")], "ci_v");
    let definition = &row[col(&cols, "definition")];
    assert!(
        definition.contains("ci_child") && definition.to_uppercase().contains("SELECT"),
        "pg_views.definition must be the stored view body, got {definition:?}"
    );
}

/// `information_schema.views` is the SQL-standard rendering of the same rows.
#[test]
fn information_schema_views_exposes_the_view_definition() {
    let db = db_with_a_view();
    let (rows, cols) = both_families(&db, "SELECT * FROM information_schema.views");
    assert_eq!(rows.len(), 1, "exactly one view was created; got {rows:?}");

    let row = &rows[0];
    assert_eq!(row[col(&cols, "table_catalog")], "heliosdb");
    assert_eq!(row[col(&cols, "table_schema")], "public");
    assert_eq!(row[col(&cols, "table_name")], "ci_v");
    assert!(
        row[col(&cols, "view_definition")].contains("ci_child"),
        "view_definition must be the stored body, got {:?}",
        row[col(&cols, "view_definition")]
    );
    // Nano's views are read-only; claiming otherwise would be a lie an ORM acts on.
    assert_eq!(row[col(&cols, "is_updatable")], "NO");
    assert_eq!(row[col(&cols, "is_insertable_into")], "NO");
}

/// PostgreSQL lists views in `information_schema.tables` with
/// `table_type = 'VIEW'`. Omitting them made every ORM that enumerates
/// relations through this view miss every view in the database.
#[test]
fn information_schema_tables_lists_views_as_table_type_view() {
    let db = db_with_a_view();
    let (rows, cols) = both_families(&db, "SELECT * FROM information_schema.tables");
    let name_idx = col(&cols, "table_name");
    let type_idx = col(&cols, "table_type");

    let base = rows
        .iter()
        .find(|r| r[name_idx] == "ci_child")
        .expect("the base table must still be listed");
    assert_eq!(base[type_idx], "BASE TABLE");

    let view = rows
        .iter()
        .find(|r| r[name_idx] == "ci_v")
        .unwrap_or_else(|| panic!("the view must be listed; got {rows:?}"));
    assert_eq!(view[type_idx], "VIEW");
}

/// `pg_class` gains `relkind='v'` rows whose OIDs come from the dedicated 7000
/// base and therefore cannot collide with table ('r'), index ('i') or sequence
/// ('S') OIDs, and `pg_attribute` describes the view's columns under the SAME
/// OID so `pg_class.oid = pg_attribute.attrelid` resolves.
#[test]
fn pg_class_and_pg_attribute_describe_views() {
    let db = db_with_a_view();
    db.execute("CREATE INDEX ci_child_qty_idx ON ci_child(qty)")
        .expect("create index");

    let (rows, cols) = both_families(&db, "SELECT * FROM pg_class");
    let oid_idx = col(&cols, "oid");
    let name_idx = col(&cols, "relname");
    let kind_idx = col(&cols, "relkind");

    let view_row = rows
        .iter()
        .find(|r| r[name_idx] == "ci_v")
        .unwrap_or_else(|| panic!("pg_class must contain the view; got {rows:?}"));
    assert_eq!(view_row[kind_idx], "v", "a view must report relkind='v'");
    let view_oid: i64 = view_row[oid_idx].parse().expect("oid is an integer");
    assert!(view_oid >= 7000, "view OIDs come from the 7000 base, got {view_oid}");

    // Disjointness: no non-view relation may share the view's OID.
    for r in &rows {
        if r[kind_idx] != "v" {
            let other: i64 = r[oid_idx].parse().expect("oid is an integer");
            assert_ne!(
                other, view_oid,
                "view OID {view_oid} collides with a {} relation named {}",
                r[kind_idx], r[name_idx]
            );
        }
    }

    // pg_attribute must describe the view's columns under that same OID.
    let (attrs, acols) = both_families(&db, "SELECT * FROM pg_attribute");
    let relid_idx = col(&acols, "attrelid");
    let attname_idx = col(&acols, "attname");
    let mut view_cols: Vec<String> = attrs
        .iter()
        .filter(|r| r[relid_idx].parse::<i64>().ok() == Some(view_oid))
        .map(|r| r[attname_idx].clone())
        .collect();
    view_cols.sort();
    assert_eq!(
        view_cols,
        vec!["id".to_string(), "qty".to_string()],
        "pg_attribute must list the view's columns under attrelid = its pg_class oid"
    );
}

/// `pg_class.relnamespace` for a view resolves through `pg_namespace` — the
/// JOIN psql `\d` and SQLAlchemy perform. Asserted two ways: as an actual JOIN,
/// and (below) by comparing the two views' OIDs directly.
#[test]
fn pg_class_view_row_joins_to_pg_namespace() {
    let db = db_with_a_view();
    let (rows, cols) = both_families(
        &db,
        "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'v'",
    );
    assert_eq!(rows.len(), 1, "exactly one view row should join; got {rows:?}");
    assert_eq!(rows[0][col(&cols, "nspname")], "public");
}

/// The same invariant without depending on JOIN support: the view's
/// `relnamespace` must equal `public`'s `pg_namespace.oid`.
#[test]
fn view_relnamespace_matches_pg_namespace_oid_for_public() {
    let db = db_with_a_view();
    let (nsp, ncols) = both_families(&db, "SELECT oid, nspname FROM pg_namespace");
    let public_oid = nsp
        .iter()
        .find(|r| r[col(&ncols, "nspname")] == "public")
        .map(|r| r[col(&ncols, "oid")].clone())
        .expect("pg_namespace must list public");

    let (cls, ccols) = both_families(&db, "SELECT relname, relkind, relnamespace FROM pg_class");
    let view = cls
        .iter()
        .find(|r| r[col(&ccols, "relname")] == "ci_v")
        .expect("pg_class must list the view");
    assert_eq!(view[col(&ccols, "relkind")], "v");
    assert_eq!(
        view[col(&ccols, "relnamespace")],
        public_oid,
        "a view's relnamespace must be the schema's pg_namespace.oid"
    );
}

/// DROP VIEW removes the view from every surface at once — the point of having
/// ONE implementation.
#[test]
fn drop_view_removes_it_from_every_surface() {
    let db = db_with_a_view();
    db.execute("DROP VIEW ci_v").expect("drop view");

    let (pgv, _) = both_families(&db, "SELECT * FROM pg_views");
    assert!(pgv.is_empty(), "pg_views must be empty after DROP VIEW; got {pgv:?}");

    let (isv, _) = both_families(&db, "SELECT * FROM information_schema.views");
    assert!(isv.is_empty(), "information_schema.views must be empty; got {isv:?}");

    let (tabs, tcols) = both_families(&db, "SELECT * FROM information_schema.tables");
    let name_idx = col(&tcols, "table_name");
    assert!(
        !tabs.iter().any(|r| r[name_idx] == "ci_v"),
        "the dropped view must not remain in information_schema.tables"
    );

    let (cls, ccols) = both_families(&db, "SELECT * FROM pg_class");
    let kind_idx = col(&ccols, "relkind");
    assert!(
        !cls.iter().any(|r| r[kind_idx] == "v"),
        "no relkind='v' row may survive DROP VIEW"
    );
}

// ---------------------------------------------------------------------------
// P4 — CHECK clauses.
// ---------------------------------------------------------------------------

/// The CHECK matrix: named, anonymous, several on one table, and a table with
/// none. `check_clause` must be the SQL predicate, not the internal
/// serde_json/`Debug` encoding of the stored `LogicalExpr`.
#[test]
fn check_constraints_expose_the_clause() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE ck_named (qty INT, CONSTRAINT ck_qty_positive CHECK (qty > 0))")
        .expect("named check");
    db.execute("CREATE TABLE ck_anon (qty INT CHECK (qty > 0))")
        .expect("anonymous check");
    db.execute(
        "CREATE TABLE ck_multi (a INT, b INT, \
         CONSTRAINT ck_a_pos CHECK (a > 0), CONSTRAINT ck_b_pos CHECK (b > 10))",
    )
    .expect("multiple checks");
    db.execute("CREATE TABLE ck_none (a INT)").expect("no constraints");

    let (rows, cols) = both_families(&db, "SELECT * FROM information_schema.check_constraints");
    let name_idx = col(&cols, "constraint_name");
    let clause_idx = col(&cols, "check_clause");
    let schema_idx = col(&cols, "constraint_schema");
    let catalog_idx = col(&cols, "constraint_catalog");

    let clause_of = |name: &str| -> String {
        rows.iter()
            .find(|r| r[name_idx] == name)
            .map(|r| r[clause_idx].clone())
            .unwrap_or_else(|| panic!("constraint `{name}` missing; got {rows:?}"))
    };

    for expected in ["ck_qty_positive", "ck_a_pos", "ck_b_pos"] {
        let clause = clause_of(expected);
        assert!(
            clause.contains('>'),
            "`{expected}`.check_clause must be SQL, got {clause:?}"
        );
        assert!(
            !clause.contains("BinaryExpr") && !clause.contains('{'),
            "`{expected}`.check_clause must not leak the internal encoding, got {clause:?}"
        );
    }
    assert!(clause_of("ck_qty_positive").contains("qty"));
    assert!(clause_of("ck_a_pos").contains('a'));
    assert!(clause_of("ck_b_pos").contains("10"));

    // The anonymous column-level CHECK is still reported, under the generated name.
    let anon = clause_of("ck_anon_check");
    assert!(
        anon.contains("qty") && anon.contains('>'),
        "an anonymous CHECK must still surface its clause, got {anon:?}"
    );

    // A table with no constraints contributes nothing.
    assert!(
        !rows.iter().any(|r| r[name_idx].starts_with("ck_none")),
        "a table with no CHECK must contribute zero rows; got {rows:?}"
    );

    // Catalog/schema columns are populated, not NULL.
    for r in &rows {
        assert_eq!(r[catalog_idx], "heliosdb");
        assert_eq!(r[schema_idx], "public");
    }
}

// ---------------------------------------------------------------------------
// P1 / P7 — schemata and catalog_name.
// ---------------------------------------------------------------------------

/// `information_schema.schemata` enumerates REAL schemas — including one
/// declared by `CREATE SCHEMA` that holds no tables — from the same source
/// `pg_namespace` uses. The deleted wire copy returned three hardcoded rows and
/// could never show `app`.
#[test]
fn schemata_enumerates_real_schemas_including_empty_ones() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE SCHEMA ci_app").expect("create schema");
    db.execute("CREATE SCHEMA ci_empty").expect("create empty schema");
    db.execute("CREATE TABLE ci_app.t (c INT)").expect("create table");

    let (rows, cols) = both_families(&db, "SELECT * FROM information_schema.schemata");
    let name_idx = col(&cols, "schema_name");
    let names: Vec<&String> = rows.iter().map(|r| &r[name_idx]).collect();

    for expected in ["public", "information_schema", "pg_catalog", "ci_app", "ci_empty"] {
        assert!(
            names.iter().any(|n| n.as_str() == expected),
            "schemata must list `{expected}`; got {names:?}"
        );
    }
    for r in &rows {
        assert_eq!(r[col(&cols, "catalog_name")], "heliosdb");
    }

    // schemata and pg_namespace are two renderings of ONE enumeration; they must
    // never disagree about which schemas exist.
    let (nsp, ncols) = both_families(&db, "SELECT nspname FROM pg_namespace");
    let mut nsp_names: Vec<String> = nsp.iter().map(|r| r[col(&ncols, "nspname")].clone()).collect();
    let mut schemata_names: Vec<String> = names.into_iter().cloned().collect();
    nsp_names.sort();
    schemata_names.sort();
    assert_eq!(
        schemata_names, nsp_names,
        "information_schema.schemata and pg_namespace must enumerate the same schemas"
    );
}

/// `information_schema.tables.table_schema` reports the real schema and
/// `table_name` the bare table — the wire copy used to hardcode `public` and
/// emit the raw `app.t` storage key as the table name.
#[test]
fn information_schema_tables_splits_the_schema_qualifier() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE SCHEMA ci_app").expect("create schema");
    db.execute("CREATE TABLE ci_app.t (c INT)").expect("create table");

    let (rows, cols) = both_families(&db, "SELECT * FROM information_schema.tables");
    let schema_idx = col(&cols, "table_schema");
    let name_idx = col(&cols, "table_name");
    let row = rows
        .iter()
        .find(|r| r[schema_idx] == "ci_app")
        .unwrap_or_else(|| panic!("a table in schema ci_app must report table_schema='ci_app'; got {rows:?}"));
    assert_eq!(
        row[name_idx], "t",
        "table_name must be the bare table, never the `schema.table` storage key"
    );
}

/// `information_schema.catalog_name` is a one-row view. It used to raise the
/// unknown-view ERROR, so SQLAlchemy-style optional probes raised instead of
/// degrading.
#[test]
fn catalog_name_returns_exactly_one_row() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let (rows, cols) = both_families(&db, "SELECT * FROM information_schema.catalog_name");
    assert_eq!(rows.len(), 1, "catalog_name is a one-row view; got {rows:?}");
    assert_eq!(rows[0][col(&cols, "catalog_name")], "heliosdb");
}

// ---------------------------------------------------------------------------
// P5 / P6 — pg_matviews and pg_indexes on the embedded route.
// ---------------------------------------------------------------------------

/// `pg_indexes` used to exist ONLY on the PG wire; the embedded / REPL / Python
/// routes errored with "does not exist".
#[test]
fn pg_indexes_is_reachable_and_lists_real_indexes() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE ci_idx (id INT PRIMARY KEY, email TEXT)")
        .expect("create table");
    db.execute("CREATE INDEX ci_idx_email ON ci_idx(email)")
        .expect("create index");

    let (rows, cols) = both_families(&db, "SELECT * FROM pg_indexes");
    let name_idx = col(&cols, "indexname");
    let def_idx = col(&cols, "indexdef");
    let table_idx = col(&cols, "tablename");

    let pkey = rows
        .iter()
        .find(|r| r[name_idx] == "ci_idx_pkey")
        .unwrap_or_else(|| panic!("the primary-key index must be listed; got {rows:?}"));
    assert_eq!(pkey[table_idx], "ci_idx");
    assert!(pkey[def_idx].contains("UNIQUE"), "pkey indexdef: {:?}", pkey[def_idx]);

    let manual = rows
        .iter()
        .find(|r| r[name_idx] == "ci_idx_email")
        .unwrap_or_else(|| panic!("the manual secondary index must be listed; got {rows:?}"));
    assert!(
        manual[def_idx].contains("USING btree") && manual[def_idx].contains("email"),
        "manual indexdef: {:?}",
        manual[def_idx]
    );
}

/// `pg_matviews` returned `Ok(vec![])` by construction on every route while a
/// working implementation sat unreachable in the dead legacy registry.
#[test]
fn pg_matviews_lists_materialized_views() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let (empty, _) = both_families(&db, "SELECT * FROM pg_matviews");
    assert!(empty.is_empty(), "no materialized views yet; got {empty:?}");

    db.execute("CREATE TABLE ci_mv_base (id INT PRIMARY KEY, n INT)")
        .expect("base table");
    db.execute("INSERT INTO ci_mv_base VALUES (1, 10), (2, 20)")
        .expect("seed");
    db.execute("CREATE MATERIALIZED VIEW ci_mv AS SELECT id, n FROM ci_mv_base")
        .expect("create materialized view");

    let (rows, cols) = both_families(&db, "SELECT * FROM pg_matviews");
    assert_eq!(rows.len(), 1, "exactly one materialized view; got {rows:?}");
    assert_eq!(rows[0][col(&cols, "matviewname")], "ci_mv");
    assert_eq!(rows[0][col(&cols, "schemaname")], "public");
    let definition = &rows[0][col(&cols, "definition")];
    assert!(
        definition.contains("ci_mv_base"),
        "pg_matviews.definition must be the stored query text, got {definition:?}"
    );
}

// ---------------------------------------------------------------------------
// Negative / empty matrix.
// ---------------------------------------------------------------------------

/// On a fresh database every newly-registered view answers with ZERO ROWS and
/// no error, on both families. An error here would break any tool that probes
/// the catalog before creating anything.
#[test]
fn empty_database_catalog_views_return_zero_rows_without_error() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    for view in [
        "pg_views",
        "pg_matviews",
        "information_schema.views",
        "information_schema.check_constraints",
    ] {
        let (rows, _) = both_families(&db, &format!("SELECT * FROM {view}"));
        assert!(
            rows.is_empty(),
            "{view} must be empty on a fresh database; got {rows:?}"
        );
    }
    // pg_indexes has no user indexes yet but must still ANSWER rather than error.
    let (idx, _) = both_families(&db, "SELECT * FROM pg_indexes");
    assert!(
        idx.is_empty(),
        "pg_indexes must be empty on a fresh database; got {idx:?}"
    );
}

/// A genuinely unknown catalog relation must still fail LOUDLY on both
/// families — an empty result would let a typo pass for "no such objects".
#[test]
fn unknown_information_schema_view_still_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let sql = "SELECT * FROM information_schema.nonexistent_xyz";
    assert!(
        db.query_with_columns(sql).is_err(),
        "an unknown information_schema view must error on the text family"
    );
    assert!(
        db.query_params_with_columns(sql, &[]).is_err(),
        "an unknown information_schema view must error on the params family"
    );
}

/// A view whose columns are nullable / not-nullable must render `is_nullable`
/// and `column_default` honestly — NULL for "no default", never an empty string.
#[test]
fn information_schema_columns_renders_nullability_and_defaults() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE ci_nulls (a INT NOT NULL DEFAULT 7, b TEXT)")
        .expect("create table");

    let (rows, cols) = both_families(
        &db,
        "SELECT column_name, is_nullable, column_default FROM information_schema.columns \
         WHERE table_name = 'ci_nulls'",
    );
    assert_eq!(cols.len(), 3, "exactly the three requested columns; got {cols:?}");
    assert_eq!(rows.len(), 2, "two columns on ci_nulls; got {rows:?}");

    let name_idx = col(&cols, "column_name");
    let null_idx = col(&cols, "is_nullable");
    let def_idx = col(&cols, "column_default");

    let a = rows.iter().find(|r| r[name_idx] == "a").expect("column a");
    assert_eq!(a[null_idx], "NO");
    assert!(
        a[def_idx].contains('7'),
        "a's default should read back as 7, got {:?}",
        a[def_idx]
    );

    let b = rows.iter().find(|r| r[name_idx] == "b").expect("column b");
    assert_eq!(b[null_idx], "YES");
    assert_eq!(b[def_idx], "NULL", "a column with no default must report NULL");
}

/// The exact ORM introspection query that used to return ZERO rows over the
/// wire (and works embedded) must return rows and EXACTLY the six requested
/// columns, with and without spaces around `=` — the two historical failure
/// shapes. This is the embedded floor; `wire_tests.rs` asserts the same over
/// the protocol, which is where the bug actually lived.
#[test]
fn orm_columns_introspection_query_returns_six_columns_and_rows() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE ci_orm (id INT PRIMARY KEY, name TEXT)")
        .expect("create table");

    for sql in [
        "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns WHERE table_schema = 'public'",
        "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns WHERE table_schema='public'",
    ] {
        let (rows, cols) = both_families(&db, sql);
        assert_eq!(
            cols,
            vec![
                "table_schema".to_string(),
                "table_name".to_string(),
                "column_name".to_string(),
                "data_type".to_string(),
                "is_nullable".to_string(),
                "column_default".to_string(),
            ],
            "the projection must be exactly the six requested columns for `{sql}`"
        );
        assert!(
            rows.iter().any(|r| r[1] == "ci_orm"),
            "the query must return the user table's columns for `{sql}`; got {rows:?}"
        );
        assert!(
            rows.iter().all(|r| r[0] == "public"),
            "WHERE table_schema = 'public' must actually filter; got {rows:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Restart / persistence.
// ---------------------------------------------------------------------------

/// View bodies, schemas and CHECK clauses are PERSISTED, so reopening the same
/// data directory must re-serve identical catalog rows. A regression here would
/// mean introspection only works until the first restart.
#[test]
fn catalog_rows_survive_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let db = EmbeddedDatabase::new(dir.path()).expect("open");
        db.execute("CREATE SCHEMA ci_persist").expect("create schema");
        db.execute("CREATE TABLE ci_p (id INT PRIMARY KEY, qty INT, CONSTRAINT ci_p_qty CHECK (qty > 0))")
            .expect("create table");
        db.execute("CREATE VIEW ci_p_v AS SELECT id FROM ci_p")
            .expect("create view");
    }

    let db = EmbeddedDatabase::new(dir.path()).expect("reopen");

    let (views, vcols) = both_families(&db, "SELECT * FROM pg_views");
    assert_eq!(views.len(), 1, "the view must survive the reopen; got {views:?}");
    assert_eq!(views[0][col(&vcols, "viewname")], "ci_p_v");
    assert_eq!(
        views[0][col(&vcols, "schemaname")],
        "public",
        "a view stored without a creator schema reports public, never NULL"
    );
    assert!(views[0][col(&vcols, "definition")].contains("ci_p"));

    let (schemata, scols) = both_families(&db, "SELECT schema_name FROM information_schema.schemata");
    assert!(
        schemata.iter().any(|r| r[col(&scols, "schema_name")] == "ci_persist"),
        "the declared schema must survive the reopen; got {schemata:?}"
    );

    let (checks, ccols) = both_families(&db, "SELECT * FROM information_schema.check_constraints");
    let ck = checks
        .iter()
        .find(|r| r[col(&ccols, "constraint_name")] == "ci_p_qty")
        .unwrap_or_else(|| panic!("the CHECK must survive the reopen; got {checks:?}"));
    assert!(
        ck[col(&ccols, "check_clause")].contains("qty"),
        "check_clause after reopen: {:?}",
        ck[col(&ccols, "check_clause")]
    );
}
