//! Comprehensive tests for PostgreSQL-compatible system views
//!
//! Tests all 18 system views across categories:
//! - Core catalog views
//! - Session and activity views
//! - v2.0 feature views
//! - v2.1 feature views

use heliosdb_lite::{
    Config, StorageEngine, Schema, Column, DataType, Value, Tuple,
    sql::{SystemViewRegistry, SessionRegistry, ProtocolType},
};

#[test]
fn test_system_view_registry_initialization() {
    let registry = SystemViewRegistry::new();

    // Verify all 18 views are registered
    let all_views = registry.list_views();
    assert!(all_views.len() >= 18, "Expected at least 18 views, got {}", all_views.len());

    // Verify specific views exist
    let expected_views = vec![
        // Core catalog (8)
        "pg_tables", "pg_views", "pg_indexes", "pg_attribute",
        "pg_database", "pg_namespace", "pg_class", "pg_type",
        // Session/Activity (3)
        "pg_stat_activity", "pg_stat_database", "pg_settings",
        // v2.0 Features (3)
        "pg_branches", "pg_matviews", "pg_snapshots",
        // v2.1 Features (4)
        "pg_stat_ssl", "pg_authid", "pg_stat_optimizer", "pg_compression_stats",
    ];

    for view_name in expected_views {
        assert!(
            registry.is_system_view(view_name),
            "Expected view '{}' to be registered",
            view_name
        );
    }
}

#[test]
fn test_pg_tables_basic() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create test tables
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("name", DataType::Text),
    ]);

    storage.catalog().create_table("users", schema.clone()).unwrap();
    storage.catalog().create_table("products", schema.clone()).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_tables", &storage).unwrap();

    // Should have at least 2 tables
    assert!(results.len() >= 2, "Expected at least 2 tables");

    // Verify structure
    for tuple in &results {
        assert_eq!(tuple.values.len(), 8, "pg_tables should have 8 columns");

        // schemaname should be "public"
        if let Value::String(schema) = &tuple.values[0] {
            assert_eq!(schema, "public");
        } else {
            panic!("Expected schemaname to be a string");
        }

        // tablename should be a string
        assert!(matches!(&tuple.values[1], Value::String(_)));
    }
}

#[test]
fn test_pg_tables_excludes_system_tables() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create regular table
    let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);
    storage.catalog().create_table("regular_table", schema.clone()).unwrap();

    // Create system-like table (should be excluded)
    storage.catalog().create_table("helios_internal", schema).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_tables", &storage).unwrap();

    // Verify helios_ tables are excluded
    for tuple in &results {
        if let Value::String(tablename) = &tuple.values[1] {
            assert!(
                !tablename.starts_with("helios_"),
                "System table '{}' should be excluded from pg_tables",
                tablename
            );
        }
    }
}

#[test]
fn test_pg_indexes_vector_indexes() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create table with vector column
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("embedding", DataType::Vector(128)),
    ]);
    storage.catalog().create_table("documents", schema).unwrap();

    // Create vector index
    use heliosdb_lite::vector::DistanceMetric;
    storage.vector_indexes().create_index(
        "doc_embedding_idx".to_string(),
        "documents".to_string(),
        "embedding".to_string(),
        128,
        DistanceMetric::Cosine,
    ).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_indexes", &storage).unwrap();

    // Should have the vector index
    assert!(results.len() >= 1, "Expected at least 1 index");

    // Verify structure
    for tuple in &results {
        assert_eq!(tuple.values.len(), 5, "pg_indexes should have 5 columns");

        // Verify indexdef contains CREATE INDEX
        if let Value::String(indexdef) = &tuple.values[4] {
            assert!(indexdef.contains("CREATE INDEX"));
        }
    }
}

#[test]
fn test_pg_attribute_column_metadata() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create table with various column types
    let schema = Schema::new(vec![
        Column {
            name: "id".to_string(),
            data_type: DataType::Int4,
            nullable: false,
            primary_key: true,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: heliosdb_lite::ColumnStorageMode::Default,
        },
        Column {
            name: "name".to_string(),
            data_type: DataType::Text,
            nullable: true,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: heliosdb_lite::ColumnStorageMode::Default,
        },
        Column {
            name: "age".to_string(),
            data_type: DataType::Int2,
            nullable: false,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: heliosdb_lite::ColumnStorageMode::Default,
        },
    ]);

    storage.catalog().create_table("test_attrs", schema).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_attribute", &storage).unwrap();

    // Should have 3 columns
    assert!(results.len() >= 3, "Expected at least 3 columns");

    // Verify structure
    let mut found_nullable = false;
    let mut found_notnull = false;

    for tuple in &results {
        assert_eq!(tuple.values.len(), 7, "pg_attribute should have 7 columns");

        // Check attnotnull field
        if let Value::Boolean(notnull) = &tuple.values[5] {
            if *notnull {
                found_notnull = true;
            } else {
                found_nullable = true;
            }
        }
    }

    assert!(found_nullable, "Should have at least one nullable column");
    assert!(found_notnull, "Should have at least one not-null column");
}

#[test]
fn test_pg_database_single_database() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_database", &storage).unwrap();

    // Should return exactly 1 database
    assert_eq!(results.len(), 1, "Expected exactly 1 database");

    // Verify structure
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), 7, "pg_database should have 7 columns");

    // datname should be "heliosdb"
    if let Value::String(datname) = &tuple.values[0] {
        assert_eq!(datname, "heliosdb");
    } else {
        panic!("Expected datname to be a string");
    }

    // datallowconn should be true
    if let Value::Boolean(allow_conn) = &tuple.values[6] {
        assert!(*allow_conn);
    }
}

#[test]
fn test_pg_namespace_public_schema() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_namespace", &storage).unwrap();

    // Should have at least the public schema
    assert!(results.len() >= 1, "Expected at least 1 namespace");

    // Find public schema
    let public_schema = results.iter().find(|t| {
        matches!(&t.values[0], Value::String(name) if name == "public")
    });

    assert!(public_schema.is_some(), "Expected to find 'public' schema");
}

#[test]
fn test_pg_class_tables_and_matviews() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create regular table
    let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);
    storage.catalog().create_table("regular_table", schema).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_class", &storage).unwrap();

    // Should have at least 1 relation
    assert!(results.len() >= 1, "Expected at least 1 relation");

    // Verify structure
    for tuple in &results {
        assert_eq!(tuple.values.len(), 7, "pg_class should have 7 columns");

        // relkind should be 'r' or 'm'
        if let Value::String(relkind) = &tuple.values[2] {
            assert!(
                relkind == "r" || relkind == "m",
                "relkind should be 'r' (table) or 'm' (matview), got '{}'",
                relkind
            );
        }
    }
}

#[test]
fn test_pg_type_builtin_types() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_type", &storage).unwrap();

    // Should have multiple built-in types
    assert!(results.len() >= 10, "Expected at least 10 built-in types");

    // Verify we have common types
    let type_names: Vec<String> = results.iter()
        .filter_map(|t| match &t.values[0] {
            Value::String(name) => Some(name.clone()),
            _ => None,
        })
        .collect();

    let expected_types = vec!["bool", "int4", "int8", "text", "timestamp"];
    for expected in expected_types {
        assert!(
            type_names.contains(&expected.to_string()),
            "Expected type '{}' not found",
            expected
        );
    }
}

#[test]
fn test_pg_stat_activity_with_sessions() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create session registry
    let session_registry = std::sync::Arc::new(SessionRegistry::new());

    // Register some sessions
    session_registry.register_session(
        ProtocolType::PostgreSQL,
        "user1".to_string(),
        "127.0.0.1".to_string(),
        5432,
    ).unwrap();

    session_registry.register_session(
        ProtocolType::Oracle,
        "user2".to_string(),
        "192.168.1.100".to_string(),
        1521,
    ).unwrap();

    // Create registry with session tracking
    let registry = SystemViewRegistry::with_session_registry(session_registry.clone());
    let results = registry.execute("pg_stat_activity", &storage).unwrap();

    // Should show both sessions
    assert_eq!(results.len(), 2, "Expected 2 active sessions");

    // Verify structure
    for tuple in &results {
        assert_eq!(tuple.values.len(), 12, "pg_stat_activity should have 12 columns");

        // Verify datname
        if let Value::String(datname) = &tuple.values[1] {
            assert_eq!(datname, "heliosdb");
        }

        // Verify usename is present
        assert!(matches!(&tuple.values[4], Value::String(_)));

        // Verify state is present
        assert!(matches!(&tuple.values[10], Value::String(_)));
    }
}

#[test]
fn test_pg_stat_database_statistics() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_stat_database", &storage).unwrap();

    // Should return at least 1 database stats row
    assert!(results.len() >= 1, "Expected at least 1 database stats row");

    // Verify structure
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), 12, "pg_stat_database should have 12 columns");

    // datname should be "heliosdb"
    if let Value::String(datname) = &tuple.values[1] {
        assert_eq!(datname, "heliosdb");
    }
}

#[test]
fn test_pg_settings_configuration() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_settings", &storage).unwrap();

    // Should have multiple settings
    assert!(results.len() >= 4, "Expected at least 4 settings");

    // Verify structure
    for tuple in &results {
        assert_eq!(tuple.values.len(), 10, "pg_settings should have 10 columns");

        // name should be a string
        assert!(matches!(&tuple.values[0], Value::String(_)));

        // setting should be a string
        assert!(matches!(&tuple.values[1], Value::String(_)));
    }

    // Verify we have expected settings
    let setting_names: Vec<String> = results.iter()
        .filter_map(|t| match &t.values[0] {
            Value::String(name) => Some(name.clone()),
            _ => None,
        })
        .collect();

    assert!(setting_names.contains(&"wal_enabled".to_string()));
    assert!(setting_names.contains(&"time_travel_enabled".to_string()));
}

#[test]
fn test_pg_branches_database_branching() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create a test branch
    use heliosdb_lite::storage::BranchOptions;
    storage.create_branch(
        "feature_branch",
        Some("main"),
        BranchOptions::default(),
    ).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_branches", &storage).unwrap();

    // Should have at least 2 branches (main + feature)
    assert!(results.len() >= 2, "Expected at least 2 branches");

    // Verify structure
    for tuple in &results {
        assert_eq!(tuple.values.len(), 9, "pg_branches should have 9 columns");

        // branch_id should be an int
        assert!(matches!(&tuple.values[0], Value::Int8(_)));

        // branch_name should be a string
        assert!(matches!(&tuple.values[1], Value::String(_)));

        // state should be a string
        assert!(matches!(&tuple.values[6], Value::String(_)));
    }
}

#[test]
fn test_pg_matviews_materialized_views() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create a base table
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("value", DataType::Int4),
    ]);
    storage.catalog().create_table("base_table", schema.clone()).unwrap();

    // Insert some data
    storage.insert_tuple("base_table", Tuple::new(vec![
        Value::Int4(1),
        Value::Int4(100),
    ])).unwrap();

    // Create a materialized view
    use heliosdb_lite::sql::LogicalPlan;
    use heliosdb_lite::storage::MaterializedViewMetadata;
    use std::sync::Arc;

    let query_plan = LogicalPlan::Scan {
        alias: None,
        table_name: "base_table".to_string(),
        schema: Arc::new(schema.clone()),
        projection: None,
        as_of: None,
    };

    let mv_catalog = storage.mv_catalog();
    let metadata = MaterializedViewMetadata::new(
        "test_mv".to_string(),
        "SELECT * FROM base_table".to_string(),
        bincode::serialize(&query_plan).unwrap(),
        vec!["base_table".to_string()],
        schema,
    );
    mv_catalog.create_view(metadata).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_matviews", &storage).unwrap();

    // Should have 1 materialized view
    assert_eq!(results.len(), 1, "Expected 1 materialized view");

    // Verify structure
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), 10, "pg_matviews should have 10 columns");

    // matviewname should be "test_mv"
    if let Value::String(matviewname) = &tuple.values[1] {
        assert_eq!(matviewname, "test_mv");
    }

    // ispopulated should be false (not refreshed yet)
    if let Value::Boolean(populated) = &tuple.values[4] {
        assert!(!populated);
    }
}

#[test]
fn test_pg_snapshots_time_travel() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create a snapshot
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    storage.snapshot_manager().register_snapshot(timestamp).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_snapshots", &storage).unwrap();

    // Should have at least 1 snapshot
    assert!(results.len() >= 1, "Expected at least 1 snapshot");

    // Verify structure
    for tuple in &results {
        assert_eq!(tuple.values.len(), 7, "pg_snapshots should have 7 columns");

        // snapshot_id should be an int
        assert!(matches!(&tuple.values[0], Value::Int8(_)));

        // is_automatic should be a boolean
        assert!(matches!(&tuple.values[6], Value::Boolean(_)));
    }
}

#[test]
fn test_pg_stat_ssl_connection_info() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create session registry with sessions
    let session_registry = std::sync::Arc::new(SessionRegistry::new());
    session_registry.register_session(
        ProtocolType::PostgreSQL,
        "user1".to_string(),
        "127.0.0.1".to_string(),
        5432,
    ).unwrap();

    let registry = SystemViewRegistry::with_session_registry(session_registry);
    let results = registry.execute("pg_stat_ssl", &storage).unwrap();

    // Should have 1 session
    assert_eq!(results.len(), 1, "Expected 1 session");

    // Verify structure
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), 8, "pg_stat_ssl should have 8 columns");

    // ssl should be false (not implemented yet)
    if let Value::Boolean(ssl) = &tuple.values[1] {
        assert!(!ssl);
    }
}

#[test]
fn test_pg_authid_users() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_authid", &storage).unwrap();

    // Should have at least 1 user
    assert!(results.len() >= 1, "Expected at least 1 user");

    // Verify structure
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), 8, "pg_authid should have 8 columns");

    // rolname should be a string
    assert!(matches!(&tuple.values[0], Value::String(_)));

    // rolsuper should be a boolean
    assert!(matches!(&tuple.values[1], Value::Boolean(_)));
}

#[test]
fn test_pg_compression_stats() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Create table
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("data", DataType::Text),
    ]);
    storage.catalog().create_table("compressed_table", schema).unwrap();

    // Set compression config
    use heliosdb_lite::storage::compression::CompressionConfig;
    use std::collections::HashMap;
    let compression_config = CompressionConfig {
        enabled: true,
        alp_enabled: true,
        min_rows_for_compression: 100,
        compression_level: 3,
        min_data_size: 1024,
        min_compression_ratio: 1.2,
        column_overrides: HashMap::new(),
        adaptive_compression: true,
    };
    storage.catalog().set_compression_config("compressed_table", &compression_config).unwrap();

    // Set mock compression stats
    use heliosdb_lite::storage::compression::CompressionStats;
    let stats = CompressionStats {
        total_original_size: 10000,
        total_compressed_size: 2500,
        overall_ratio: 4.0,
        column_stats: HashMap::new(),
    };
    storage.catalog().set_compression_stats("compressed_table", &stats).unwrap();

    let registry = SystemViewRegistry::new();
    let results = registry.execute("pg_compression_stats", &storage).unwrap();

    // Should have 1 table with compression stats
    assert_eq!(results.len(), 1, "Expected 1 table with compression stats");

    // Verify structure
    let tuple = &results[0];
    assert_eq!(tuple.values.len(), 9, "pg_compression_stats should have 9 columns");

    // Verify compression ratio calculation
    if let Value::Float8(ratio) = &tuple.values[5] {
        assert!(*ratio > 1.0, "Compression ratio should be > 1.0");
        assert!((*ratio - 4.0).abs() < 0.1, "Expected ratio ~4.0, got {}", ratio);
    }
}

#[test]
fn test_all_views_have_valid_schemas() {
    let registry = SystemViewRegistry::new();

    for view_name in registry.list_views() {
        let schema = registry.get_schema(view_name)
            .unwrap_or_else(|| panic!("View '{}' should have a schema", view_name));

        assert!(
            !schema.columns.is_empty(),
            "View '{}' should have at least one column",
            view_name
        );

        // Verify all columns have names
        for (idx, column) in schema.columns.iter().enumerate() {
            assert!(
                !column.name.is_empty(),
                "View '{}' column {} should have a name",
                view_name,
                idx
            );
        }
    }
}

#[test]
fn test_execute_nonexistent_view() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    let registry = SystemViewRegistry::new();
    let result = registry.execute("pg_nonexistent", &storage);

    assert!(result.is_err(), "Querying nonexistent view should return error");

    if let Err(e) = result {
        assert!(
            e.to_string().contains("Unknown system view"),
            "Error message should mention unknown view"
        );
    }
}

#[test]
fn test_view_categories() {
    let registry = SystemViewRegistry::new();

    use heliosdb_lite::sql::ViewCategory;

    let core_views = registry.list_views_by_category(ViewCategory::Core);
    assert!(core_views.len() >= 6, "Should have at least 6 core views");

    let session_views = registry.list_views_by_category(ViewCategory::Session);
    assert!(session_views.len() >= 1, "Should have at least 1 session view");

    let feature_views = registry.list_views_by_category(ViewCategory::Feature);
    assert!(feature_views.len() >= 3, "Should have at least 3 feature views");

    let stats_views = registry.list_views_by_category(ViewCategory::Statistics);
    assert!(stats_views.len() >= 2, "Should have at least 2 statistics views");
}

#[test]
fn test_integration_full_workflow() {
    // Comprehensive integration test
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();

    // Setup: Create tables, indexes, branches, materialized views
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("name", DataType::Text),
        Column::new("embedding", DataType::Vector(128)),
    ]);

    storage.catalog().create_table("products", schema.clone()).unwrap();

    // Insert data
    storage.insert_tuple("products", Tuple::new(vec![
        Value::Int4(1),
        Value::String("Product 1".to_string()),
        Value::Vector(vec![0.1; 128]),
    ])).unwrap();

    // Create vector index
    use heliosdb_lite::vector::DistanceMetric;
    storage.vector_indexes().create_index(
        "products_embedding_idx".to_string(),
        "products".to_string(),
        "embedding".to_string(),
        128,
        DistanceMetric::Cosine,
    ).unwrap();

    // Create branch
    use heliosdb_lite::storage::BranchOptions;
    storage.create_branch(
        "dev_branch",
        Some("main"),
        BranchOptions::default(),
    ).unwrap();

    // Create session
    let session_registry = std::sync::Arc::new(SessionRegistry::new());
    session_registry.register_session(
        ProtocolType::PostgreSQL,
        "testuser".to_string(),
        "127.0.0.1".to_string(),
        5432,
    ).unwrap();

    let registry = SystemViewRegistry::with_session_registry(session_registry);

    // Test all views work
    let views_to_test = vec![
        "pg_tables", "pg_indexes", "pg_attribute", "pg_database",
        "pg_stat_activity", "pg_settings", "pg_branches",
    ];

    for view_name in views_to_test {
        let results = registry.execute(view_name, &storage)
            .unwrap_or_else(|e| panic!("Failed to execute {}: {}", view_name, e));

        assert!(
            !results.is_empty() || view_name == "pg_views",
            "View '{}' should return results",
            view_name
        );
    }
}
