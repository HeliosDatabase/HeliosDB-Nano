#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
#![allow(clippy::assert_is_empty)]
//! The PostgreSQL-compatible system views, read from the registry that answers
//! real queries (`sql::phase3::SystemViewRegistry`).
//!
//! These tests used to assert a second registry that no statement could reach.
//! That registry is deleted; every test of a view that exists in the live
//! registry was ported here, reading columns by name. The tests of views that
//! existed only in the deleted registry (`pg_stat_database`, `pg_branches`,
//! `pg_snapshots`, `pg_stat_ssl`, `pg_stat_optimizer`, `pg_compression_stats`,
//! and the view-category listing) were removed with it.

use heliosdb_nano::{
    sql::{dict::DictContext, phase3::SystemViewRegistry},
    Column, Config, DataType, EmbeddedDatabase, Schema, StorageEngine, Tuple, Value,
};

fn storage() -> StorageEngine {
    StorageEngine::open_in_memory(&Config::in_memory()).unwrap()
}

/// The rows of `view` over a bare storage engine.
fn rows_of(view: &str, storage: &StorageEngine) -> Vec<Tuple> {
    SystemViewRegistry::shared()
        .execute(view, &DictContext::new(storage))
        .unwrap_or_else(|e| panic!("{view} must execute: {e}"))
}

/// Position of `column` in `view`.
fn col(view: &str, column: &str) -> usize {
    let schema = SystemViewRegistry::shared()
        .get_schema(view)
        .unwrap_or_else(|| panic!("{view} must be registered"));
    schema
        .columns
        .iter()
        .position(|c| c.name == column)
        .unwrap_or_else(|| panic!("{view} must have a column {column}"))
}

fn text(tuple: &Tuple, idx: usize) -> String {
    match &tuple.values[idx] {
        Value::String(s) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn width(view: &str) -> usize {
    SystemViewRegistry::shared().get_schema(view).unwrap().columns.len()
}

#[test]
fn test_system_view_registry_initialization() {
    let registry = SystemViewRegistry::shared();
    assert!(registry.list_views().len() >= 18);
    for view in [
        "pg_tables",
        "pg_views",
        "pg_indexes",
        "pg_attribute",
        "pg_database",
        "pg_namespace",
        "pg_class",
        "pg_type",
        "pg_stat_activity",
        "pg_settings",
        "pg_matviews",
        "pg_authid",
    ] {
        assert!(registry.is_system_view(view), "expected view '{view}' to be registered");
    }
}

#[test]
fn test_pg_tables_basic() {
    let storage = storage();
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("name", DataType::Text),
    ]);
    storage.catalog().create_table("users", schema.clone()).unwrap();
    storage.catalog().create_table("products", schema).unwrap();

    let results = rows_of("pg_tables", &storage);
    assert_eq!(results.len(), 2, "two tables were created");
    let (schemaname, tablename) = (col("pg_tables", "schemaname"), col("pg_tables", "tablename"));
    for tuple in &results {
        assert_eq!(tuple.values.len(), 8, "pg_tables has PostgreSQL's 8 columns");
        assert_eq!(text(tuple, schemaname), "public");
    }
    let mut names: Vec<String> = results.iter().map(|t| text(t, tablename)).collect();
    names.sort();
    assert_eq!(names, ["products", "users"]);
}

#[test]
fn test_pg_tables_excludes_system_tables() {
    let storage = storage();
    let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);
    storage.catalog().create_table("regular_table", schema.clone()).unwrap();
    storage
        .catalog()
        .create_table("helios_internal", schema.clone())
        .unwrap();
    // A user table whose name starts with `mv_` is a user table and is listed.
    storage.catalog().create_table("mv_user_table", schema).unwrap();

    let tablename = col("pg_tables", "tablename");
    let names: Vec<String> = rows_of("pg_tables", &storage)
        .iter()
        .map(|t| text(t, tablename))
        .collect();
    assert!(names.contains(&"regular_table".to_string()));
    assert!(names.contains(&"mv_user_table".to_string()));
    assert!(
        !names.iter().any(|name| name.starts_with("helios_")),
        "bookkeeping tables are excluded from pg_tables: {names:?}"
    );
}

#[test]
fn test_pg_indexes_vector_indexes() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE documents (id INT4 PRIMARY KEY, embedding VECTOR(3))")
        .unwrap();
    db.execute("CREATE INDEX doc_embedding_idx ON documents USING hnsw (embedding vector_cosine_ops)")
        .unwrap();

    let results = SystemViewRegistry::shared()
        .execute("pg_indexes", &DictContext::for_engine(&db.storage))
        .unwrap();
    let (indexname, indexdef) = (col("pg_indexes", "indexname"), col("pg_indexes", "indexdef"));
    let vector_index = results
        .iter()
        .find(|t| text(t, indexname) == "doc_embedding_idx")
        .expect("the vector index is listed");
    for tuple in &results {
        assert_eq!(tuple.values.len(), 5, "pg_indexes has 5 columns");
        assert!(text(tuple, indexdef).contains("INDEX"));
    }
    assert!(text(vector_index, indexdef).contains("CREATE INDEX"));
    assert!(text(vector_index, indexdef).contains("hnsw"));
}

#[test]
fn test_pg_attribute_column_metadata() {
    let storage = storage();
    let column = |name: &str, data_type: DataType, nullable: bool, primary_key: bool| Column {
        name: name.to_string(),
        data_type,
        nullable,
        primary_key,
        source_table: None,
        source_table_name: None,
        default_expr: None,
        unique: false,
        storage_mode: heliosdb_nano::ColumnStorageMode::Default,
    };
    let schema = Schema::new(vec![
        column("id", DataType::Int4, false, true),
        column("name", DataType::Text, true, false),
        column("age", DataType::Int2, false, false),
    ]);
    storage.catalog().create_table("test_attrs", schema).unwrap();

    let results = rows_of("pg_attribute", &storage);
    assert_eq!(results.len(), 3, "one row per column");
    let (attname, attnotnull, attnum) = (
        col("pg_attribute", "attname"),
        col("pg_attribute", "attnotnull"),
        col("pg_attribute", "attnum"),
    );
    let described: Vec<(String, Value, Value)> = results
        .iter()
        .map(|t| (text(t, attname), t.values[attnotnull].clone(), t.values[attnum].clone()))
        .collect();
    assert_eq!(
        described,
        vec![
            ("id".to_string(), Value::Boolean(true), Value::Int4(1)),
            ("name".to_string(), Value::Boolean(false), Value::Int4(2)),
            ("age".to_string(), Value::Boolean(true), Value::Int4(3)),
        ]
    );
    for tuple in &results {
        assert_eq!(tuple.values.len(), width("pg_attribute"));
    }
}

#[test]
fn test_pg_database_single_database() {
    let storage = storage();
    let results = rows_of("pg_database", &storage);
    assert_eq!(results.len(), 1, "exactly one database before any CREATE DATABASE");
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), width("pg_database"));
    assert_eq!(text(tuple, col("pg_database", "datname")), "heliosdb");
    assert_eq!(tuple.values[col("pg_database", "datallowconn")], Value::Boolean(true));
}

#[test]
fn test_pg_namespace_public_schema() {
    let storage = storage();
    let nspname = col("pg_namespace", "nspname");
    let results = rows_of("pg_namespace", &storage);
    assert!(
        results.iter().any(|t| text(t, nspname) == "public"),
        "expected to find the 'public' schema"
    );
}

#[test]
fn test_pg_class_tables_and_matviews() {
    let storage = storage();
    let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);
    storage.catalog().create_table("regular_table", schema.clone()).unwrap();
    // The storage of a materialized view is listed as the view, relkind `m`.
    storage.catalog().create_table("__mv_some_view", schema).unwrap();

    let results = rows_of("pg_class", &storage);
    let (relname, relkind) = (col("pg_class", "relname"), col("pg_class", "relkind"));
    let kinds: Vec<(String, String)> = results.iter().map(|t| (text(t, relname), text(t, relkind))).collect();
    assert!(
        kinds.contains(&("regular_table".to_string(), "r".to_string())),
        "{kinds:?}"
    );
    assert!(kinds.contains(&("some_view".to_string(), "m".to_string())), "{kinds:?}");
    for tuple in &results {
        assert_eq!(tuple.values.len(), width("pg_class"));
        let kind = text(tuple, relkind);
        assert!(
            kind == "r" || kind == "m",
            "only a table and a materialized view exist, got '{kind}'"
        );
    }
}

/// HDB-011: the full PostgreSQL type inventory, read by column name.
#[test]
fn test_pg_type_builtin_types() {
    let storage = storage();
    let results = rows_of("pg_type", &storage);
    let name_idx = col("pg_type", "typname");

    assert!(
        results.len() >= 40,
        "Expected at least 40 pg_type rows, got {}",
        results.len()
    );
    let type_names: Vec<String> = results.iter().map(|t| text(t, name_idx)).collect();
    for expected in [
        "bool",
        "int2",
        "int4",
        "int8",
        "text",
        "varchar",
        "float4",
        "float8",
        "timestamp",
        "uuid",
        "json",
        "jsonb",
        "numeric",
        "date",
        // Array types are part of the inventory, linked by typarray/typelem.
        "_int4",
    ] {
        assert!(
            type_names.contains(&expected.to_string()),
            "Expected type '{expected}' not found"
        );
    }
}

/// HDB-011: `pg_range` and `pg_enum` are registered and empty, so the
/// introspection every driver runs alongside `pg_type` resolves instead of
/// erroring. tokio-postgres' TYPEINFO lookup `LEFT OUTER JOIN`s `pg_range`;
/// drizzle-kit and Prisma join `pg_enum` to read enum labels. Both tolerate
/// zero rows; neither tolerates "relation does not exist".
#[test]
fn test_pg_range_and_pg_enum_are_registered_and_empty() {
    let storage = storage();
    let registry = SystemViewRegistry::shared();

    for (view, columns) in [
        (
            "pg_range",
            vec![
                "rngtypid",
                "rngsubtype",
                "rngmultitypid",
                "rngcollation",
                "rngsubopc",
                "rngcanonical",
                "rngsubdiff",
            ],
        ),
        ("pg_enum", vec!["oid", "enumtypid", "enumsortorder", "enumlabel"]),
    ] {
        assert!(registry.is_system_view(view), "{view} must be a registered view");
        let schema = registry
            .get_schema(view)
            .unwrap_or_else(|| panic!("{view} must have a schema"));
        let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, columns, "{view} must use PostgreSQL's column names");
        let rows = rows_of(view, &storage);
        assert!(rows.is_empty(), "{view} must answer zero rows, got {}", rows.len());
    }
}

/// `pg_stat_activity` lists the live backends of the database. The deleted
/// registry filled it from a session table of its own; the live view reads the
/// backends that exist.
#[test]
fn test_pg_stat_activity_answers_with_postgresql_columns() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let (rows, columns) = db
        .query_with_columns("SELECT pid, usename, application_name, state FROM pg_stat_activity")
        .unwrap();
    assert_eq!(columns, ["pid", "usename", "application_name", "state"]);
    for row in &rows {
        assert!(matches!(row.values[0], Value::Int4(_)), "pid is an integer: {row:?}");
    }
}

#[test]
fn test_pg_settings_configuration() {
    let storage = storage();
    let results = rows_of("pg_settings", &storage);
    assert!(results.len() >= 4, "Expected at least 4 settings");
    let (name, setting) = (col("pg_settings", "name"), col("pg_settings", "setting"));
    for tuple in &results {
        assert_eq!(tuple.values.len(), width("pg_settings"));
        assert!(matches!(&tuple.values[name], Value::String(_)));
    }
    let names: Vec<String> = results.iter().map(|t| text(t, name)).collect();
    for expected in ["server_version", "max_connections", "search_path"] {
        assert!(names.contains(&expected.to_string()), "{expected} is a setting");
    }
    let version = results
        .iter()
        .find(|t| text(t, name) == "server_version")
        .expect("server_version");
    assert!(matches!(&version.values[setting], Value::String(v) if !v.is_empty()));
}

#[test]
fn test_pg_matviews_materialized_views() {
    use heliosdb_nano::sql::LogicalPlan;
    use heliosdb_nano::storage::MaterializedViewMetadata;
    use std::sync::Arc;

    let storage = storage();
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("value", DataType::Int4),
    ]);
    storage.catalog().create_table("base_table", schema.clone()).unwrap();
    storage
        .insert_tuple("base_table", Tuple::new(vec![Value::Int4(1), Value::Int4(100)]))
        .unwrap();

    let query_plan = LogicalPlan::Scan {
        alias: None,
        table_name: "base_table".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };
    let metadata = MaterializedViewMetadata::new(
        "test_mv".to_string(),
        "SELECT * FROM base_table".to_string(),
        bincode::serialize(&query_plan).unwrap(),
        vec!["base_table".to_string()],
        schema,
    );
    storage.mv_catalog().create_view(metadata).unwrap();

    let results = rows_of("pg_matviews", &storage);
    assert_eq!(results.len(), 1, "Expected 1 materialized view");
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), width("pg_matviews"));
    assert_eq!(text(tuple, col("pg_matviews", "matviewname")), "test_mv");
    assert_eq!(text(tuple, col("pg_matviews", "schemaname")), "public");
    // Not refreshed yet.
    assert_eq!(tuple.values[col("pg_matviews", "ispopulated")], Value::Boolean(false));
    assert_eq!(
        text(tuple, col("pg_matviews", "definition")),
        "SELECT * FROM base_table"
    );
}

#[test]
fn test_pg_authid_users() {
    let storage = storage();
    let results = rows_of("pg_authid", &storage);
    assert!(!results.is_empty(), "Expected at least 1 role");
    for tuple in &results {
        assert_eq!(tuple.values.len(), width("pg_authid"));
        assert!(matches!(&tuple.values[col("pg_authid", "rolname")], Value::String(_)));
        assert!(matches!(&tuple.values[col("pg_authid", "rolsuper")], Value::Boolean(_)));
    }
}

#[test]
fn test_all_views_have_valid_schemas() {
    let registry = SystemViewRegistry::shared();
    for view_name in registry.list_views() {
        let schema = registry
            .get_schema(view_name)
            .unwrap_or_else(|| panic!("View '{view_name}' should have a schema"));
        assert!(
            !schema.columns.is_empty(),
            "View '{view_name}' should have at least one column"
        );
        for (idx, column) in schema.columns.iter().enumerate() {
            assert!(
                !column.name.is_empty(),
                "View '{view_name}' column {idx} should have a name"
            );
        }
    }
}

#[test]
fn test_execute_nonexistent_view() {
    let storage = storage();
    let error = SystemViewRegistry::shared()
        .execute("pg_nonexistent", &DictContext::new(&storage))
        .expect_err("a name that is not a system view is an error");
    assert!(error.to_string().contains("Unknown system view"), "{error}");
}

#[test]
fn test_integration_full_workflow() {
    use heliosdb_nano::storage::BranchOptions;

    let storage = storage();
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("name", DataType::Text),
    ]);
    storage.catalog().create_table("products", schema).unwrap();
    storage
        .insert_tuple(
            "products",
            Tuple::new(vec![Value::Int4(1), Value::String("Product 1".to_string())]),
        )
        .unwrap();
    storage
        .create_branch("dev_branch", Some("main"), BranchOptions::default())
        .unwrap();

    for view in [
        "pg_tables",
        "pg_class",
        "pg_attribute",
        "pg_database",
        "pg_namespace",
        "pg_settings",
        "pg_type",
        "pg_authid",
    ] {
        let results = rows_of(view, &storage);
        assert!(!results.is_empty(), "View '{view}' should return results");
        for tuple in &results {
            assert_eq!(tuple.values.len(), width(view), "{view}: one value per column");
        }
    }
    // Views that are empty here still answer.
    for view in ["pg_views", "pg_indexes", "pg_matviews", "pg_proc", "pg_stat_activity"] {
        let results = rows_of(view, &storage);
        for tuple in &results {
            assert_eq!(tuple.values.len(), width(view), "{view}: one value per column");
        }
    }
}
