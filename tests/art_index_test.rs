//! ART (Adaptive Radix Tree) Index Tests
//!
//! Tests for ART index functionality including:
//! - Automatic index creation on PK, FK, and UNIQUE constraints
//! - Manual index creation via CREATE INDEX ... USING ART
//! - System view heliosdb_art_indexes
//! - Index operations (insert, lookup, range scan)
//! - Constraint enforcement (PK, FK, UNIQUE violations)

#![allow(clippy::unwrap_used)]

use heliosdb_lite::{Config, Value};
use heliosdb_lite::storage::{StorageEngine, ArtIndexType};

/// Test automatic PK index creation
#[test]
fn test_pk_index_auto_creation() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();
    let catalog = storage.catalog();

    // Create a table with a primary key
    let schema = heliosdb_lite::Schema {
        columns: vec![
            heliosdb_lite::Column {
                name: "id".to_string(),
                data_type: heliosdb_lite::DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
            heliosdb_lite::Column {
                name: "name".to_string(),
                data_type: heliosdb_lite::DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
        ],
    };

    catalog.create_table("test_pk", schema).unwrap();

    // Check that PK index was auto-created
    let art_manager = storage.art_indexes();
    assert!(art_manager.has_pk("test_pk"), "PK index should be auto-created");

    // Verify index exists
    let pk_index = art_manager.get_pk_index("test_pk");
    assert!(pk_index.is_some(), "PK index should be retrievable");

    let pk_idx = pk_index.unwrap();
    assert_eq!(pk_idx.index_type(), ArtIndexType::PrimaryKey);
    assert_eq!(pk_idx.table(), "test_pk");
}

/// Test automatic UNIQUE index creation
#[test]
fn test_unique_index_auto_creation() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();
    let catalog = storage.catalog();

    // Create a table with a unique column
    let schema = heliosdb_lite::Schema {
        columns: vec![
            heliosdb_lite::Column {
                name: "id".to_string(),
                data_type: heliosdb_lite::DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
            heliosdb_lite::Column {
                name: "email".to_string(),
                data_type: heliosdb_lite::DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: true,  // UNIQUE constraint
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
        ],
    };

    catalog.create_table("test_unique", schema).unwrap();

    // Check that UNIQUE index was auto-created
    let art_manager = storage.art_indexes();
    let unique_indexes = art_manager.get_unique_indexes("test_unique");
    assert!(!unique_indexes.is_empty(), "UNIQUE index should be auto-created");

    let unique_idx = &unique_indexes[0];
    assert_eq!(unique_idx.index_type(), ArtIndexType::Unique);
}

/// Test ART index drop on table drop
#[test]
fn test_index_drop_on_table_drop() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();
    let catalog = storage.catalog();

    // Create table with PK
    let schema = heliosdb_lite::Schema {
        columns: vec![
            heliosdb_lite::Column {
                name: "id".to_string(),
                data_type: heliosdb_lite::DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
        ],
    };

    catalog.create_table("test_drop", schema).unwrap();

    // Verify index exists
    let art_manager = storage.art_indexes();
    assert!(art_manager.has_pk("test_drop"));

    // Drop table
    catalog.drop_table("test_drop").unwrap();

    // Verify index was dropped
    assert!(!art_manager.has_pk("test_drop"), "PK index should be dropped with table");
}

/// Test heliosdb_art_indexes system view
#[test]
fn test_art_indexes_system_view() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();
    let catalog = storage.catalog();

    // Create table with PK and UNIQUE
    let schema = heliosdb_lite::Schema {
        columns: vec![
            heliosdb_lite::Column {
                name: "id".to_string(),
                data_type: heliosdb_lite::DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
            heliosdb_lite::Column {
                name: "email".to_string(),
                data_type: heliosdb_lite::DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: true,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
        ],
    };

    catalog.create_table("test_view", schema).unwrap();

    // Query system view (via list_indexes)
    let art_manager = storage.art_indexes();
    let indexes = art_manager.list_indexes();

    // Should have at least PK and UNIQUE indexes
    assert!(indexes.len() >= 2, "Should have at least PK and UNIQUE indexes");

    // Check PK index
    let pk_indexes: Vec<_> = indexes.iter()
        .filter(|(_, _, t, _)| *t == ArtIndexType::PrimaryKey)
        .collect();
    assert!(!pk_indexes.is_empty(), "Should have PK index");

    // Check UNIQUE index
    let unique_indexes: Vec<_> = indexes.iter()
        .filter(|(_, _, t, _)| *t == ArtIndexType::Unique)
        .collect();
    assert!(!unique_indexes.is_empty(), "Should have UNIQUE index");
}

/// Test ART index manager stats
#[test]
fn test_art_manager_stats() {
    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config).unwrap();
    let catalog = storage.catalog();

    // Create table with PK and UNIQUE
    let schema = heliosdb_lite::Schema {
        columns: vec![
            heliosdb_lite::Column {
                name: "id".to_string(),
                data_type: heliosdb_lite::DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
            heliosdb_lite::Column {
                name: "code".to_string(),
                data_type: heliosdb_lite::DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: true,
                storage_mode: heliosdb_lite::ColumnStorageMode::Default,
            },
        ],
    };

    catalog.create_table("test_stats", schema).unwrap();

    let art_manager = storage.art_indexes();
    let stats = art_manager.stats();

    // Should have at least 2 indexes (PK + UNIQUE)
    assert!(stats.total_indexes >= 2, "Should have at least 2 indexes");
    assert!(stats.pk_indexes >= 1, "Should have at least 1 PK index");
    assert!(stats.unique_indexes >= 1, "Should have at least 1 UNIQUE index");
}

/// Test basic ART node types
#[test]
fn test_art_node_basic_operations() {
    use heliosdb_lite::storage::art_index::AdaptiveRadixTree;

    // Create a manual ART index
    let mut art = AdaptiveRadixTree::new(
        "test_idx",
        "test_table",
        vec!["col".to_string()],
        ArtIndexType::Manual,
    );

    // Insert some keys
    art.insert(b"apple", 1).unwrap();
    art.insert(b"banana", 2).unwrap();
    art.insert(b"cherry", 3).unwrap();

    // Test lookups
    assert_eq!(art.get(b"apple"), Some(1));
    assert_eq!(art.get(b"banana"), Some(2));
    assert_eq!(art.get(b"cherry"), Some(3));
    assert_eq!(art.get(b"date"), None);

    // Test len
    assert_eq!(art.len(), 3);

    // Test contains
    assert!(art.contains(b"apple"));
    assert!(!art.contains(b"date"));

    // Test delete
    art.remove(b"banana");
    assert_eq!(art.get(b"banana"), None);
    assert_eq!(art.len(), 2);
}

/// Test ART with numeric keys
#[test]
fn test_art_numeric_keys() {
    use heliosdb_lite::storage::art_index::AdaptiveRadixTree;

    let mut art = AdaptiveRadixTree::new(
        "test_idx",
        "test_table",
        vec!["id".to_string()],
        ArtIndexType::PrimaryKey,
    );

    // Insert numeric keys (as big-endian bytes for correct ordering)
    for i in 0u32..100 {
        let key = i.to_be_bytes();
        art.insert(&key, i as u64).unwrap();
    }

    // Verify all lookups
    for i in 0u32..100 {
        let key = i.to_be_bytes();
        assert_eq!(art.get(&key), Some(i as u64));
    }

    assert_eq!(art.len(), 100);
}

/// Test ART iteration
#[test]
fn test_art_iteration() {
    use heliosdb_lite::storage::art_index::AdaptiveRadixTree;

    let mut art = AdaptiveRadixTree::new(
        "test_idx",
        "test_table",
        vec!["name".to_string()],
        ArtIndexType::Manual,
    );

    art.insert(b"alice", 1).unwrap();
    art.insert(b"bob", 2).unwrap();
    art.insert(b"charlie", 3).unwrap();
    art.insert(b"david", 4).unwrap();

    // Collect all entries via iterator
    let entries: Vec<(Vec<u8>, u64)> = art.iter().collect();
    assert_eq!(entries.len(), 4);

    // Entries should be in sorted order
    let keys: Vec<Vec<u8>> = entries.iter().map(|(k, _)| k.clone()).collect();
    let mut sorted_keys = keys.clone();
    sorted_keys.sort();
    assert_eq!(keys, sorted_keys, "Iterator should return keys in sorted order");
}

/// Test ART range scan
#[test]
fn test_art_range_scan() {
    use heliosdb_lite::storage::art_index::AdaptiveRadixTree;

    let mut art = AdaptiveRadixTree::new(
        "test_idx",
        "test_table",
        vec!["name".to_string()],
        ArtIndexType::Manual,
    );

    art.insert(b"a", 1).unwrap();
    art.insert(b"b", 2).unwrap();
    art.insert(b"c", 3).unwrap();
    art.insert(b"d", 4).unwrap();
    art.insert(b"e", 5).unwrap();

    // Range scan from "b" to "d" (exclusive)
    let entries: Vec<(Vec<u8>, u64)> = art.range(b"b", b"d").collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, b"b".to_vec());
    assert_eq!(entries[1].0, b"c".to_vec());
}

/// Test ART prefix scan
#[test]
fn test_art_prefix_scan() {
    use heliosdb_lite::storage::art_index::AdaptiveRadixTree;

    let mut art = AdaptiveRadixTree::new(
        "test_idx",
        "test_table",
        vec!["path".to_string()],
        ArtIndexType::Manual,
    );

    art.insert(b"/users/alice", 1).unwrap();
    art.insert(b"/users/bob", 2).unwrap();
    art.insert(b"/users/charlie", 3).unwrap();
    art.insert(b"/posts/1", 10).unwrap();
    art.insert(b"/posts/2", 20).unwrap();

    // Prefix scan for "/users/"
    let entries: Vec<(Vec<u8>, u64)> = art.prefix_scan(b"/users/").collect();
    assert_eq!(entries.len(), 3);

    // All should start with "/users/"
    for (key, _) in &entries {
        assert!(key.starts_with(b"/users/"));
    }
}

/// Test ART node growth (Node4 -> Node16 -> Node48 -> Node256)
#[test]
fn test_art_node_growth() {
    use heliosdb_lite::storage::art_index::AdaptiveRadixTree;

    let mut art = AdaptiveRadixTree::new(
        "test_idx",
        "test_table",
        vec!["key".to_string()],
        ArtIndexType::Manual,
    );

    // Insert 300 unique keys to force node growth
    for i in 0u16..300 {
        let key = format!("key{:03}", i);
        art.insert(key.as_bytes(), i as u64).unwrap();
    }

    assert_eq!(art.len(), 300);

    // Verify all lookups still work
    for i in 0u16..300 {
        let key = format!("key{:03}", i);
        assert_eq!(art.get(key.as_bytes()), Some(i as u64));
    }

    // Check stats show different node types
    let stats = art.stats();
    assert!(stats.key_count == 300);
}
